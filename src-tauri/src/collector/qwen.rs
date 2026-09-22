//! Qwen Code 适配器（M2-11）：tail `<runtime>/projects/<sanitizeCwd>/chats/<uuid>.jsonl` 转录。
//! 调研依据：01-RESEARCH §13（QwenLM/qwen-code v0.24.4 源码级核实，2026-09-23）。
//! 要点：
//! - fork 基线 gemini-cli v0.8.2 且自 v0.1 起停止同步——落盘结构与 Gemini CLI 已
//!   大幅分叉，故独立适配器不做同族参数化（调研推翻总纲预想）。
//! - 转录为纯追加消息树（uuid/parentUuid，形态接近 Claude Code）：每行自带
//!   cwd/version/gitBranch；文件名即会话 uuid。
//! - token：assistant 行 usageMetadata（promptTokenCount 等，与上游同字段名），
//!   值按协议归一化后 cached ⊆ prompt 统一成立——input −= cached 拆分入库；
//!   thoughtsTokenCount 可能是思考文本估算值（当参考值用）。
//! - error 信号：转录行无稳定错误形态（subtype=turn_result 待装机核实），
//!   依赖 hooks 的 StopFailure/PostToolUseFailure（事件清单见 hooks 模块）＋
//!   OTel outfile 的 api_error（M2-12 增强通道，见 collector/otel.rs）。
//! - hooks：22 事件 CC 式（settings.json `hooks` 键），事件名与 CC 几乎全同名
//!   （PermissionRequest/StopFailure/PostToolUseFailure 直通状态机）；
//!   ⚠️ Qwen timeout 单位秒（≥1000 按旧毫秒语义读），支持 async/shell 字段。

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

/// 配置根（QWEN_HOME 重定向 settings/oauth 等；校验目录存在，异常回落默认并留痕）
fn qwen_home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("QWEN_HOME") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return Some(p);
        }
        log::debug!("[qwen-code] QWEN_HOME 指向的目录不存在，回落默认路径：{}", p.display());
    }
    Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join(".qwen"))
}

/// 运行时根（chats/projects 等大流量数据落点）：QWEN_RUNTIME_DIR > 配置根。
/// settings 内的 runtimeOutputDir 分支不读（极少用，装机核实后视需要补）
fn runtime_root() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("QWEN_RUNTIME_DIR") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return Some(p);
        }
        log::debug!("[qwen-code] QWEN_RUNTIME_DIR 指向的目录不存在，回落配置根：{}", p.display());
    }
    qwen_home()
}

/// 转录头部元数据（头内首行 cwd ＋首条 user 消息；两者一经写入永不变，缓存永续；
/// 会话 id 不读头部——文件名本身就是 sessionId uuid）
#[derive(Debug, Clone)]
struct HeadMeta {
    /// 项目目录（首行 cwd；projects/<sanitizeCwd> 不可逆，以行内 cwd 为准）
    project_dir: Option<String>,
    /// 标题兜底：头内首条 user 消息文本
    title: Option<String>,
}

pub struct QwenCodeAdapter {
    /// 运行时根（projects/ 的父目录）
    root: PathBuf,
    /// 目录枚举走树器（`projects/*/chats/*.jsonl` 精确三层；`*.jsonl` 天然排除
    /// 同目录的 `<sessionId>.runtime.json` 存活 sidecar 与 legacy tmp 旧目录）
    walker: Mutex<GlobWalker>,
    /// 单文件增量游标（mtime 过滤/64KB 回退/重建归零，引擎机制）
    reader: Mutex<IncrementalFileReader>,
    /// 头部元数据缓存：路径 → HeadMeta（内容永不变，不随 mtime 失效）
    head_cache: Mutex<HashMap<PathBuf, Option<HeadMeta>>>,
    /// per-file 最近模型缓存：模型随 assistant 行更新，增量读时旧行不在本轮增量内
    model_cache: Mutex<HashMap<PathBuf, String>>,
    /// OTel outfile 增强通道（M2-12，见 collector/otel.rs 模块注释；token 行暂不入库）
    otel: OtelOutfileSink,
}

impl QwenCodeAdapter {
    pub fn new() -> Self {
        // settings 在配置根（qwen_home）不在转录运行时根（runtime_root），两根
        // 不同源须分别给定；qwen_home 取不到时同空路径兜底（sink 内部全程容错）
        let settings = qwen_home().unwrap_or_else(|| PathBuf::from("")).join("settings.json");
        Self::with_roots(runtime_root().unwrap_or_else(|| PathBuf::from("")), settings)
    }

    /// 指定根目录构造（单测注入临时目录用）：settings 与转录同根
    pub fn with_root(root: PathBuf) -> Self {
        Self::with_roots(root.clone(), root.join("settings.json"))
    }

    /// 公共构造：root＝转录运行时根，settings_path＝遥测配置载体位置
    fn with_roots(root: PathBuf, settings_path: PathBuf) -> Self {
        Self {
            root,
            walker: Mutex::new(GlobWalker::default()),
            reader: Mutex::new(IncrementalFileReader::default()),
            head_cache: Mutex::new(HashMap::new()),
            model_cache: Mutex::new(HashMap::new()),
            otel: OtelOutfileSink::new(otel::QWEN_PROFILE, settings_path),
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
        self.lock_walker().list(&self.root, "projects/*/chats/*.jsonl")
    }

    /// 头部元数据（64KB 头读，缓存永续；一行都解析不出时不缓存，下轮重试）
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
        let mut project_dir = None;
        let mut title = None;
        for line in head.lines() {
            let Ok(j) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            // 首行 cwd（每行都带 cwd，取首个非空）
            if project_dir.is_none() {
                project_dir = j
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
            }
            // 首条 user 消息：message.parts 内 text 分片，或 displayText 兜底
            if title.is_none() && j.get("type").and_then(|t| t.as_str()) == Some("user") {
                let text = j.get("message").and_then(|m| m.get("parts")).and_then(|parts| {
                    parts.as_array().and_then(|arr| {
                        arr.iter().find_map(|p| p.get("text").and_then(|t| t.as_str())).map(|s| s.to_string())
                    })
                });
                title = text.filter(|s| !s.is_empty()).map(|s| s.chars().take(60).collect());
            }
            if project_dir.is_some() && title.is_some() {
                break;
            }
        }
        if project_dir.is_none() && title.is_none() {
            // 头部全空（文件尚未写完等）：不缓存，下轮重试
            return None;
        }
        let meta = HeadMeta { project_dir, title };
        self.lock_head().insert(path.to_path_buf(), Some(meta.clone()));
        Some(meta)
    }
}

impl Default for QwenCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// 转录行（ChatRecord）宽松解析：30+ subtype 不枚举，只取本适配器消费的字段
#[derive(serde::Deserialize)]
struct ChatRecordLine {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// assistant 行 token 统计（camelCase：promptTokenCount 等，与上游同字段名）
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<serde_json::Value>,
    /// 标题行候选字段（custom_title 的确切载体字段装机核实；多指针宽容提取）
    #[serde(default)]
    title: Option<String>,
}

/// usageMetadata 拆分四项：prompt ⊇ cached（多协议归一化后的统一语义，调研确证），
/// input −= cached；thoughtsTokenCount 记入 reasoning（可能是估算值，参考用）
fn split_usage(u: &serde_json::Value) -> (i64, i64, i64, i64) {
    let g = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let prompt = g("promptTokenCount");
    let cached = g("cachedContentTokenCount");
    let (input, cache_read) = if prompt >= cached { (prompt - cached, cached) } else { (prompt, cached) };
    (input, g("candidatesTokenCount"), g("thoughtsTokenCount"), cache_read)
}

impl AgentAdapter for QwenCodeAdapter {
    fn id(&self) -> &'static str {
        "qwen-code"
    }

    /// 快轮信号：projects 目录浅枚举（转录追加即活动）＋hooks 事件文件（装了才有）
    /// ＋OTel outfile（配置了才有）
    fn hot_signals(&self) -> Vec<HotSignal> {
        vec![
            HotSignal::File(std::sync::Arc::new(|| {
                super::hook_events::events_file_path("qwen-code")
            })),
            HotSignal::DirScan {
                root: self.root.join("projects"),
                ext: Some(".jsonl"),
                depth: 3,
                max_files: 400,
            },
            // OTel outfile 快轮信号：配置了 outfile 才有信号（从无到有即变化）
            self.otel.hot_signal(),
        ]
    }

    /// 进程匹配：npm 包 @qwen-code/qwen-code 经 node shim、standalone 装在
    /// %LOCALAPPDATA%\qwen-code——两形态命令行都含 qwen-code，进程名无特征；
    /// 本工具自身排除，防误判存活
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &[],
            cmd_keywords: &["qwen-code"],
            cmd_excludes: &["agenttrackerisland"],
        })
    }

    /// 会话发现：每个 <uuid>.jsonl 即一个会话；最近 90 天有修改的才纳入
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let cutoff = chrono::Utc::now().timestamp_millis() - 90 * 24 * 3600 * 1000;
        let mut out = vec![];
        for f in self.transcript_files() {
            let mtime = mtime_ms(&f);
            if mtime < cutoff {
                continue;
            }
            // 文件名即会话 uuid（官方 SESSION_FILE_PATTERN 校验 32~36 位 hex-dash，
            // 此处宽松取 stem；读不出跳过）
            let Some(session_id) = f.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // 头部元数据：全部缺失（空文件等）也允许登记会话（mtime 即活跃证据）
            let head = self.head_meta(&f).unwrap_or(HeadMeta { project_dir: None, title: None });
            out.push(SessionInfo {
                id: format!("qwen-code:{session_id}"),
                agent: "qwen-code".into(),
                provider: None, // collect 阶段按模型回填（qwen* → alibaba）
                model: None,
                project_dir: head.project_dir,
                // custom_title 经 titles 通道动态覆盖；此处为首条 user 兜底
                title: head.title,
                first_seen_at: mtime,
                last_seen_at: mtime,
                last_usage_at: Some(mtime),
            });
        }
        out.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        if out.len() > 100 {
            log::debug!("[qwen-code] 会话 {} 个，截断保留最近 100", out.len());
        }
        out.truncate(100);
        Ok(out)
    }

    /// 水位增量采集（纯追加转录，无重复行语义；幂等键防增量回退重读双计）：
    ///   assistant 行 usageMetadata key = "qw:{uuid}"；custom_title 行走 titles 通道
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        let mut rows: Vec<UsageRow> = vec![];
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
            // 会话 id = 文件名 stem；头部缺失（空文件）时以空字符串承接（后续行自会带数据）
            let Some(session_id) = f.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let namespaced = format!("qwen-code:{session_id}");
            let Some(body) = body_after_partial(&text, start) else {
                continue;
            };
            // 本轮增量内最近的模型名（跨轮由缓存承接）
            let mut model = self.lock_model().get(&f).cloned().unwrap_or_default();
            for line in body.lines() {
                let Ok(j) = serde_json::from_str::<ChatRecordLine>(line) else {
                    bad_lines += 1;
                    continue;
                };
                let kind = j.kind.as_deref().unwrap_or_default();
                // 标题行（system/custom_title）：多指针宽容提取，后写覆盖
                if kind == "system" && j.subtype.as_deref() == Some("custom_title") {
                    if let Some(t) = j.title.as_deref().filter(|s| !s.is_empty()) {
                        titles.insert(namespaced.clone(), t.to_string());
                    }
                    continue;
                }
                // 用量行：assistant 且带 usageMetadata（子代理 isSidechain 行的
                // 消耗同样真实，照采）
                if kind != "assistant" {
                    continue;
                }
                let Some(usage) = j.usage_metadata.as_ref() else {
                    continue; // 无计量（中间态/工具行）：忽略
                };
                let Some(ts) = j.timestamp.as_deref().and_then(iso_to_ms) else {
                    continue;
                };
                if ts <= watermark_ts {
                    continue;
                }
                if let Some(m) = j.model.as_deref() {
                    if !m.is_empty() {
                        model = m.to_string();
                    }
                }
                let (input, output, reasoning, cache_read) = split_usage(usage);
                // 幂等键：官方 uuid；缺失（格式漂移）时按文件＋毫秒指纹兜底
                let source = match j.uuid.as_deref() {
                    Some(id) if !id.is_empty() => format!("qw:{id}"),
                    _ => format!("qw:{}:{}", session_id, ts),
                };
                rows.push(UsageRow {
                    session_id: namespaced.clone(),
                    agent: "qwen-code".into(),
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
                });
            }
            self.lock_model().insert(f.clone(), model);
        }
        if err_open + bad_lines > 0 {
            log::debug!(
                "[qwen-code] 采集异常统计：文件打开/读取失败 {err_open}，坏行 {bad_lines}（静默容忍）"
            );
        }
        // OTel outfile 通道（M2-12）：api_error 错误信号行并入本轮去重（转录行无稳定错误
        // 形态，outfile 是本家最精确的 error 信号源）；token 行按所有者裁定（2026-09-23）
        // 暂不入库：与转录通道同回合无公共 id 可对齐，入库必双计，装机对账后若切换主通道
        // 在此处把 batch.rows 一并并入即可
        let batch = self.otel.collect();
        for e in batch.errors {
            rows.push(e);
        }
        if !batch.rows.is_empty() {
            log::debug!("[qwen-code] otel outfile：token 行 {}（暂不入库）", batch.rows.len());
        }
        rows.sort_by_key(|r| r.ts);
        Ok(CollectOutput {
            rows,
            titles: titles.into_iter().collect(),
            ..CollectOutput::default()
        })
    }
}

// ===== hooks 安装/卸载（增强档；settings.json JSON 注入，走 engine 公共注入器） =====
// 注入形状按 hookRegistry（2026-09-23 源码核对，只验证 type/必填字段存在性，
// 多余字段静默忽略）：事件键 PascalCase → [{matcher?, hooks:[{type,command,...}]}]
// ⚠️ Qwen 与 CC 的差异：timeout 单位秒（≥1000 按旧毫秒语义读）、支持 async/shell；
// async=true 立即返回零阻塞（宪法红线②），shell 显式 powershell 避开
// 「省略时 Windows 默认可能是 cmd.exe/Git Bash」的三态不确定性。

mod hooks {
    use std::path::PathBuf;

    use super::{inject_json_hooks, qwen_home, uninstall_json_hooks};

    /// 桥脚本注入标记（卸载识别自家条目；与 CC 共用同一份桥脚本源码）
    pub const BRIDGE_MARK: &str = "hook-bridge.js";
    /// 注入事件集（22 事件中状态机可消费的 10 个）：CC 同名直通集＋Qwen 增强——
    /// PermissionRequest=等批准（waiting）、StopFailure/PostToolUseFailure=回合/工具
    /// 失败（error，精确于通知文本启发式）；MessageDisplay（高频流式＋detached
    /// 特殊语义）/TodoCreated 等无增量价值不注入
    pub const HOOK_EVENTS: &[&str] = &[
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "Notification",
        "PermissionRequest",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ];

    fn settings_path() -> Option<PathBuf> {
        Some(qwen_home()?.join("settings.json"))
    }

    fn bridge_script_path() -> Option<PathBuf> {
        Some(qwen_home()?.join("hooks").join("hook-bridge.js"))
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

    /// 安装：①桥脚本写出到 ~\.qwen\hooks\hook-bridge.js（QWEN_HOME 重定向跟随）；
    /// ②settings.json 备份后合并注入 10 事件（防重复）；返回注入条数
    pub fn install_hooks() -> anyhow::Result<usize> {
        let bridge = bridge_script_path().ok_or_else(|| anyhow::anyhow!("无法定位用户目录"))?;
        if let Some(dir) = bridge.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&bridge, super::super::claude_code::BRIDGE_SOURCE)?;
        let settings = settings_path().ok_or_else(|| anyhow::anyhow!("无法定位 settings.json"))?;
        // 第二参数是 Agent 标识：桥脚本按它把事件写入各自的 events/<agent>.jsonl
        let cmd = format!("node \"{}\" qwen-code", bridge.to_string_lossy().replace('\\', "/"));
        let entry = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": cmd,
                "timeout": 10,
                "async": true,
                "shell": "powershell"
            }]
        });
        let injected = inject_json_hooks(&settings, HOOK_EVENTS, entry, BRIDGE_MARK)?;
        log::info!("[qwen-code] hooks 安装完成：注入 {injected} 个事件，桥脚本 {}", bridge.display());
        Ok(injected)
    }

    /// 卸载：移除全部自家注入条目；桥脚本文件保留（重装免复制，且无副作用）
    pub fn uninstall_hooks() -> anyhow::Result<usize> {
        let settings = settings_path().ok_or_else(|| anyhow::anyhow!("无法定位 settings.json"))?;
        let removed = uninstall_json_hooks(&settings, BRIDGE_MARK)?;
        log::info!("[qwen-code] hooks 卸载完成：移除 {removed} 个注入条目");
        Ok(removed)
    }
}

pub use hooks::{hooks_installed, install_hooks, uninstall_hooks};

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("at-qwen-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 合成转录解析：user 行（标题兜底）/assistant usageMetadata 拆分
    /// （prompt − cached）/custom_title 标题/tool_result 忽略/两项目目录并存
    #[test]
    fn test_collect_parse_lines() {
        let dir = tmp_dir("parse");
        let chats = dir.join("projects").join("f--proj-a").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let file = chats.join("b47ca510-7ac7-4d89-9123-15f132d2c4d8.jsonl");
        let lines = [
            // user 行：带 cwd（project_dir 来源）＋message.parts 文本（标题兜底）
            r#"{"uuid":"u1","parentUuid":null,"sessionId":"b47ca510-7ac7-4d89-9123-15f132d2c4d8","timestamp":"2026-09-23T10:00:01.000Z","type":"user","cwd":"F:\\proj-a","version":"0.24.4","message":{"role":"user","parts":[{"text":"帮我写个脚本"}]}}"#,
            // assistant 行：prompt=500 cached=200 → input=300；thoughts 进 reasoning
            r#"{"uuid":"a1","parentUuid":"u1","sessionId":"b47ca510-7ac7-4d89-9123-15f132d2c4d8","timestamp":"2026-09-23T10:00:02.000Z","type":"assistant","cwd":"F:\\proj-a","model":"qwen3-coder-plus","usageMetadata":{"promptTokenCount":500,"candidatesTokenCount":100,"totalTokenCount":830,"cachedContentTokenCount":200,"thoughtsTokenCount":30}}"#,
            // tool_result 行：忽略
            r#"{"uuid":"t1","parentUuid":"a1","timestamp":"2026-09-23T10:00:03.000Z","type":"tool_result","cwd":"F:\\proj-a"}"#,
            // 无 usageMetadata 的 assistant 行：忽略
            r#"{"uuid":"a2","timestamp":"2026-09-23T10:00:04.000Z","type":"assistant","cwd":"F:\\proj-a","model":"qwen3-coder-plus"}"#,
            // custom_title 标题行
            r#"{"uuid":"s1","timestamp":"2026-09-23T10:00:05.000Z","type":"system","subtype":"custom_title","title":"脚本会话","cwd":"F:\\proj-a"}"#,
            // 第二条 assistant（另一模型）
            r#"{"uuid":"a3","parentUuid":"t1","timestamp":"2026-09-23T10:00:06.000Z","type":"assistant","cwd":"F:\\proj-a","model":"qwen-max","usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5}}"#,
            // 坏行：忽略
            r#"{broken"#,
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();

        let ad = QwenCodeAdapter::with_root(dir.clone());
        let out = ad.collect_usage(0).unwrap();
        // a1＋a3 共 2 行（t1/a2 忽略）
        assert_eq!(out.rows.len(), 2);
        assert!(out.rows.iter().all(|r| r.session_id == "qwen-code:b47ca510-7ac7-4d89-9123-15f132d2c4d8"));
        let a1 = out.rows.iter().find(|r| r.source_id.as_deref() == Some("qw:a1")).unwrap();
        assert_eq!(a1.input_tokens, Some(300), "prompt 500 − cached 200 = 300");
        assert_eq!(a1.output_tokens, Some(100));
        assert_eq!(a1.reasoning_tokens, Some(30));
        assert_eq!(a1.cache_read_tokens, Some(200));
        assert_eq!(a1.model, "qwen3-coder-plus");
        assert_eq!(a1.provider.as_deref(), Some("alibaba"));
        let a3 = out.rows.iter().find(|r| r.source_id.as_deref() == Some("qw:a3")).unwrap();
        assert_eq!(a3.model, "qwen-max");
        // custom_title 标题
        assert_eq!(out.titles, vec![("qwen-code:b47ca510-7ac7-4d89-9123-15f132d2c4d8".to_string(), "脚本会话".to_string())]);

        // 会话发现：头缓存承接（cwd/首条 user 标题）
        let sessions = ad.scan_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "qwen-code:b47ca510-7ac7-4d89-9123-15f132d2c4d8");
        assert_eq!(sessions[0].project_dir.as_deref(), Some("F:\\proj-a"));
        assert_eq!(sessions[0].title.as_deref(), Some("帮我写个脚本"));
    }

    /// 水位增量：同实例第二次采集只产出新增行（锁定增量路径不回退）
    #[test]
    fn test_collect_incremental() {
        let dir = tmp_dir("incr");
        let chats = dir.join("projects").join("p").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let file = chats.join("c1a8a25b-0000-4000-8000-000000000001.jsonl");
        // 时间戳用明显过去的日期：文件级过滤按 mtime（现在）> watermark，
        // 若用"今天"的时间戳，本机时钟未到该 UTC 时刻时 mtime 会小于水位（CC 同款教训）
        let l1 = r#"{"uuid":"x1","timestamp":"2026-09-15T11:00:01.000Z","type":"assistant","cwd":"F:\\x","model":"qwen3-coder-plus","usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10}}"#;
        std::fs::write(&file, format!("{l1}\n")).unwrap();

        let ad = QwenCodeAdapter::with_root(dir.clone());
        let r1 = ad.collect_usage(0).unwrap().rows;
        assert_eq!(r1.len(), 1);
        let ts1 = chrono::DateTime::parse_from_rfc3339("2026-09-15T11:00:01.000Z")
            .unwrap()
            .timestamp_millis();

        // 追加第二条 → 只产出新增 1 行
        // （等待跨过 Windows mtime 定时器粒度：两次写间隔太近 mtime 相同，
        // changed() 会误判「无新增」——engine 自测同款处理）
        std::thread::sleep(std::time::Duration::from_millis(30));
        let l2 = r#"{"uuid":"x2","timestamp":"2026-09-15T11:00:02.000Z","type":"assistant","cwd":"F:\\x","model":"qwen3-coder-plus","usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":8}}"#;
        std::fs::write(&file, format!("{l1}\n{l2}\n")).unwrap();
        let r2 = ad.collect_usage(ts1).unwrap().rows;
        assert_eq!(r2.len(), 1, "第二次采集应只含新增行");
        assert_eq!(r2[0].source_id.as_deref(), Some("qw:x2"));

        // 文件未变化 → 0 行
        let r3 = ad.collect_usage(0).unwrap().rows;
        assert!(r3.is_empty(), "mtime 未变时应跳过文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// hooks 注入/卸载往返：临时 settings.json，验证防重复与完整还原
    /// （条目形状为 Qwen 版：timeout 秒＋async＋shell）
    #[test]
    fn test_hooks_install_uninstall_roundtrip() {
        let dir = tmp_dir("hooks");
        let settings = dir.join("settings.json");
        std::fs::write(&settings, r#"{
          "theme": "dark",
          "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "notify.cmd"}]}]
          }
        }"#).unwrap();

        let cmd = "node \"C:/x/.qwen/hooks/hook-bridge.js\" qwen-code";
        let mk_entry = || {
            serde_json::json!({
                "hooks": [{
                    "type": "command",
                    "command": cmd,
                    "timeout": 10,
                    "async": true,
                    "shell": "powershell"
                }]
            })
        };
        let n = inject_json_hooks(&settings, hooks::HOOK_EVENTS, mk_entry(), hooks::BRIDGE_MARK).unwrap();
        assert_eq!(n, 10);
        let s1: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(s1["hooks"]["Stop"].as_array().unwrap().len(), 2, "已有事件应追加");
        assert_eq!(s1["hooks"]["PermissionRequest"].as_array().unwrap().len(), 1);
        assert_eq!(s1["theme"], "dark", "其他配置不受影响");
        let injected = &s1["hooks"]["Stop"][1]["hooks"][0];
        assert_eq!(injected["timeout"], 10, "Qwen timeout 单位秒");
        assert_eq!(injected["async"], true);
        assert_eq!(injected["shell"], "powershell");

        // 重复注入：防重复，0 条
        let n2 = inject_json_hooks(&settings, hooks::HOOK_EVENTS, mk_entry(), hooks::BRIDGE_MARK).unwrap();
        assert_eq!(n2, 0);

        // 卸载：自家条目全清，用户配置原样保留
        let removed = uninstall_json_hooks(&settings, hooks::BRIDGE_MARK).unwrap();
        assert_eq!(removed, 10);
        let s2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(s2["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(!s2["hooks"].as_object().unwrap().contains_key("PermissionRequest"));
        assert_eq!(s2["theme"], "dark");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OTel outfile 通道挂载（M2-12）：settings 启用 telemetry.outfile 后，
    /// api_error 错误信号行并入 collect_usage 输出（token 全 None）；api_response
    /// token 行按所有者裁定暂不入库，不得出现在采集输出
    #[test]
    fn test_otel_error_channel_integrated() {
        let dir = tmp_dir("otqwen");
        let outfile = dir.join("telemetry.log");
        // 一条 Qwen api_error 的 pretty JSON 记录（OTel SDK 真实序列化形态：仅含
        // resource/instrumentationScope/attributes 三键，时间戳只在 attributes 里）
        let err_rec = serde_json::json!({
            "resource": {"attributes": {"service.name": "qwen-code"}},
            "instrumentationScope": {"name": "qwen-code"},
            "attributes": {
                "session.id": "q-ot-sess",
                "event.name": "api_error",
                "event.timestamp": "2026-09-15T10:00:01.000Z",
                "model": "qwen3-coder-plus",
                "error_message": "Request failed with status 500",
                "error_type": "server_error",
                "response_id": "resp-ot1",
            },
        });
        let mut content = serde_json::to_string_pretty(&err_rec).unwrap();
        content.push('\n');
        // 再拼一条 api_response token 记录：裁定暂不入库，验证其不出现
        let tok_rec = serde_json::json!({
            "resource": {"attributes": {"service.name": "qwen-code"}},
            "instrumentationScope": {"name": "qwen-code"},
            "attributes": {
                "session.id": "q-ot-sess",
                "event.name": "api_response",
                "event.timestamp": "2026-09-15T10:00:02.000Z",
                "model": "qwen3-coder-plus",
                "input_token_count": 100,
                "output_token_count": 20,
                "cached_content_token_count": 0,
                "thoughts_token_count": 5,
                "response_id": "resp-ot2",
            },
        });
        content.push_str(&serde_json::to_string_pretty(&tok_rec).unwrap());
        content.push('\n');
        std::fs::write(&outfile, &content).unwrap();

        // settings.json 把 outfile 指向同目录临时文件（路径 JSON 转义字面量）
        let settings = dir.join("settings.json");
        let settings_text = format!(
            r#"{{"telemetry": {{"enabled": true, "outfile": {}}}}}"#,
            serde_json::to_string(outfile.to_str().unwrap()).unwrap()
        );
        std::fs::write(&settings, settings_text).unwrap();

        let ad = QwenCodeAdapter::with_root(dir.clone());
        let out = ad.collect_usage(0).unwrap();
        // 恰 1 行：错误行并入；token 行（otq:resp-ot2）不出现
        assert_eq!(out.rows.len(), 1, "应只有一条错误行，实际 {:?}", out.rows);
        assert!(
            !out.rows.iter().any(|r| r.source_id.as_deref() == Some("otq:resp-ot2")),
            "token 行不应入库"
        );
        let e = &out.rows[0];
        assert_eq!(e.session_id, "qwen-code:q-ot-sess");
        assert_eq!(e.error_type.as_deref(), Some("server_error"));
        assert_eq!(e.source_id.as_deref(), Some("otq:e:resp-ot1"));
        assert_eq!(e.input_tokens, None, "错误行无 token 计量");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 集成：本机真实转录（手动：cargo test -- --ignored test_real_qwen；
    /// 装机后补跑，清单见 01-RESEARCH §13.3）
    #[test]
    #[ignore]
    fn test_real_qwen() {
        let ad = QwenCodeAdapter::new();
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 Qwen Code 会话（未装则本测试不适用）");
        let out = ad.collect_usage(0).unwrap();
        assert!(!out.rows.is_empty(), "本机应有历史用量");
        assert!(out.rows.iter().all(|u| u.session_id.starts_with("qwen-code:")));
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
