//! Kimi Code 适配器（M2-7）：tail `$KIMI_CODE_HOME/sessions/**/wire.jsonl` 转录。
//! 数据面勘察（2026-09-23 源码级核实，MoonshotAI/kimi-code v2.0.2）见
//! docs/01-RESEARCH.md §11——**纠正总纲 §1.2 的旧版记录**：当前主线是 TypeScript
//! 的 kimi-code（旧 Python kimi-cli 已归档），数据根 `~/.kimi-code`（KIMI_CODE_HOME
//! 重定向，旧 KIMI_SHARE_DIR 已不生效）：
//!   ① 会话结构 `sessions/<wd_key>/<sid>/{state.json, agents/<agentId>/wire.jsonl}`；
//!   ② 用量行 wire.jsonl 的 `type:"usage.record"`，字段 camelCase：
//!      `{agentId, model, usage:{inputOther, output, inputCacheRead, inputCacheCreation},
//!      time: Unix 毫秒}`；无 reasoning 独立字段；
//!   ③ wire 行无唯一 id → source_id 用内容指纹（agentId+time+四项用量）；
//!   ④ 会话元数据（title/cwd/updatedAt）在 state.json（camelCase，毫秒时间戳）；
//!   ⑤ hooks 20 事件，TOML 顶层 `[[hooks]]`（仅 event/matcher/command/timeout 四字段）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::engine::{
    body_after_partial, mtime_ms, GlobWalker, HotSignal, IncrementalFileReader, ProcessMatch,
};
use super::{AgentAdapter, CollectOutput, SessionInfo, provider_from_model};
use crate::store::UsageRow;

pub use hooks::{hooks_installed, install_hooks, uninstall_hooks};

/// 会话根目录（$KIMI_CODE_HOME/sessions，缺省 ~/.kimi-code/sessions）。
/// KIMI_CODE_HOME 重定向校验目录存在且可读，异常回落默认并留痕（§2.8 隐私细则 4）
fn sessions_root() -> Option<PathBuf> {
    Some(kimi_home()?.join("sessions"))
}

/// Kimi Code 数据根（KIMI_CODE_HOME 环境变量 > ~/.kimi-code）
fn kimi_home() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("KIMI_CODE_HOME") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return Some(p);
        }
        log::debug!("[kimi] KIMI_CODE_HOME 指向的目录不存在，回落默认路径：{}", p.display());
        return None;
    }
    let home = std::env::var_os("USERPROFILE")?;
    Some(PathBuf::from(home).join(".kimi-code"))
}

/// state.json 结构（SessionMeta v2，camelCase 序列化；仅取展示所需字段）
#[derive(serde::Deserialize, Clone)]
struct KimiState {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<i64>,
}

/// 读取会话目录的 state.json（不存在/损坏返回 None——Kimi 写入中途可能短暂为空）
fn read_state(session_dir: &Path) -> Option<KimiState> {
    let raw = std::fs::read_to_string(session_dir.join("state.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

pub struct KimiCodeAdapter {
    root: PathBuf,
    /// 目录枚举走树器（`**/wire.jsonl` 覆盖 wd_key/sid/agents 三层哈希目录）
    walker: Mutex<GlobWalker>,
    /// 单文件增量游标（引擎机制：mtime 过滤/64KB 回退/重建归零）
    reader: Mutex<IncrementalFileReader>,
    /// state.json 读取缓存：会话目录 →（state.json mtime，解析结果）
    state_cache: Mutex<HashMap<PathBuf, (i64, Option<KimiState>)>>,
}

impl KimiCodeAdapter {
    pub fn new() -> Self {
        Self::with_root(sessions_root().unwrap_or_else(|| PathBuf::from("")))
    }

    /// 指定根目录构造（单测注入临时目录用）
    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            walker: Mutex::new(GlobWalker::default()),
            reader: Mutex::new(IncrementalFileReader::default()),
            state_cache: Mutex::new(HashMap::new()),
        }
    }

    fn lock_walker(&self) -> MutexGuard<'_, GlobWalker> {
        self.walker.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_reader(&self) -> MutexGuard<'_, IncrementalFileReader> {
        self.reader.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_state(&self) -> MutexGuard<'_, HashMap<PathBuf, (i64, Option<KimiState>)>> {
        self.state_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wire_files(&self) -> Vec<PathBuf> {
        self.lock_walker().list(&self.root, "**/wire.jsonl")
    }
}

impl Default for KimiCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for KimiCodeAdapter {
    fn id(&self) -> &'static str {
        "kimi-code"
    }

    /// 快轮信号：①session_index.jsonl（每会话创建/更新时追加，回合起点早期信号）；
    /// ②sessions 目录浅枚举（depth 2 覆盖 sid 层的 state.json 变化）；
    /// ③hooks 事件文件（装了增强档）。wire.jsonl 在 4 层深处不做 DirScan 目标
    /// （枚举成本高）——回合中高频追加靠 1s 自适应 tick 兜底，误差 ≤1s
    fn hot_signals(&self) -> Vec<HotSignal> {
        let mut out = vec![
            HotSignal::File(std::sync::Arc::new(|| {
                Some(kimi_home()?.join("session_index.jsonl"))
            })),
            HotSignal::File(std::sync::Arc::new(|| {
                super::hook_events::events_file_path("kimi-code")
            })),
        ];
        if !self.root.as_os_str().is_empty() {
            out.push(HotSignal::DirScan {
                root: self.root.clone(),
                ext: None,
                depth: 2,
                max_files: 400,
            });
        }
        out
    }

    /// 进程匹配（M2-4 声明化）：官方脚本安装 = kimi.exe（SEA 单二进制）；
    /// npm 安装 = node shim，命令行含包名；kimi-legacy 是旧版 Python CLI 改名，排除
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &["kimi"],
            cmd_keywords: &["@moonshot-ai/kimi-code"],
            cmd_excludes: &["kimi-legacy", "agenttrackerisland"],
        })
    }

    /// 会话发现：wire.jsonl 所在会话目录即一个会话；元数据从 state.json 读
    /// （mtime 缓存，title/cwd 缺失容忍——写入中途可能短暂不完整）
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let cutoff = chrono::Utc::now().timestamp_millis() - 90 * 24 * 3600 * 1000;
        let mut out = vec![];
        for f in self.wire_files() {
            // wire 路径 .../<wd_key>/<sid>/agents/<agentId>/wire.jsonl：
            // 逐级上溯 wire→agentId→agents→<sid>，会话目录是三级父目录
            let Some(session_dir) = f
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
            else {
                continue;
            };
            let Some(session_id) = session_dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // 会话活动时间：wire 追加 mtime 与 state.json updatedAt 取较大者
            let wire_mtime = mtime_ms(&f);
            if wire_mtime < cutoff {
                continue;
            }
            let state_mtime = mtime_ms(&session_dir.join("state.json"));
            let state = {
                let mut map = self.lock_state();
                match map.get(session_dir).cloned() {
                    Some((m0, cached)) if m0 == state_mtime => cached,
                    _ => {
                        let fresh = read_state(session_dir);
                        map.insert(session_dir.to_path_buf(), (state_mtime, fresh.clone()));
                        fresh
                    }
                }
            };
            let updated_at = state.as_ref().and_then(|s| s.updated_at).unwrap_or(0);
            out.push(SessionInfo {
                id: format!("kimi-code:{session_id}"),
                agent: "kimi-code".into(),
                provider: None, // collect 阶段按模型回填（kimi/moonshot → moonshot）
                model: None,    // collect 阶段按 usage.record 回填（service 层兜底）
                project_dir: state.as_ref().and_then(|s| s.cwd.clone()),
                title: state.as_ref().and_then(|s| s.title.clone()).filter(|t| !t.is_empty()),
                first_seen_at: wire_mtime.min(updated_at).max(0),
                last_seen_at: wire_mtime.max(updated_at),
                // 状态聚合的活动信号：state.json 的 updatedAt 在 turn 结束边界更新，
                // 与 wire mtime（请求级追加）互补——长回答期间两者皆可能静止，由
                // hooks 增强档（TurnStarted 等）补齐，未装 hooks 时靠快轮信号
                last_usage_at: Some(wire_mtime.max(updated_at)),
            });
        }
        out.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        if out.len() > 100 {
            log::debug!("[kimi] 会话 {} 个，截断保留最近 100", out.len());
        }
        out.truncate(100);
        Ok(out)
    }

    /// 水位增量采集：解析 usage.record 行（字段 camelCase，见模块注释②③）。
    /// source_id 内容指纹 = "kr:{agentId}:{time}:{四项}"——wire 行无官方 id，
    /// 同指纹（同毫秒同模型同用量）合并为一次调用，碰撞概率与误差均可忽略；
    /// usageScope 按每请求一行假设全量入库（装机对账验证点，见 01-RESEARCH §11）
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        let mut dedup: HashMap<String, UsageRow> = HashMap::new();
        let (mut err_open, mut bad_lines) = (0usize, 0usize);
        for f in self.wire_files() {
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
            let Some(body) = body_after_partial(&text, start) else {
                continue;
            };
            for line in body.lines() {
                let Ok(j) = serde_json::from_str::<serde_json::Value>(line) else {
                    bad_lines += 1;
                    continue;
                };
                if j.get("type").and_then(|t| t.as_str()) != Some("usage.record") {
                    continue; // metadata/turn.* 等其他 wire 行忽略
                }
                let Some(ts) = j.get("time").and_then(|t| t.as_i64()) else {
                    continue; // 无时间戳无法进时间线，跳过（容忍）
                };
                if ts <= watermark_ts {
                    continue;
                }
                let Some(usage) = j.get("usage") else { continue };
                let g = |k: &str| usage.get(k).and_then(|v| v.as_i64());
                let agent_id = j.get("agentId").and_then(|a| a.as_str()).unwrap_or("main");
                let model = j.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
                // 会话 id：wire 路径反推（同 scan_sessions，三级父目录）
                let Some(session_dir) = f
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                else {
                    continue;
                };
                let Some(session_id) = session_dir.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let input = g("inputOther").unwrap_or(0);
                let output = g("output").unwrap_or(0);
                let cache_read = g("inputCacheRead").unwrap_or(0);
                let cache_creation = g("inputCacheCreation").unwrap_or(0);
                let source_id =
                    format!("kr:{agent_id}:{ts}:{input}:{output}:{cache_read}:{cache_creation}");
                let row = UsageRow {
                    session_id: format!("kimi-code:{session_id}"),
                    agent: "kimi-code".into(),
                    model: model.to_string(),
                    provider: provider_from_model(model),
                    ts,
                    input_tokens: Some(input),
                    output_tokens: Some(output),
                    reasoning_tokens: None, // TokenUsage 无 reasoning 独立字段（调研②）
                    cache_read_tokens: Some(cache_read),
                    cache_creation_tokens: Some(cache_creation),
                    duration_ms: None,
                    ttft_ms: None,
                    error_type: None,
                    source_id: Some(source_id.clone()),
                    is_background: false,
                };
                // 同指纹重复行（增量回退重读）：保留即可（值相同）
                dedup.entry(source_id).or_insert(row);
            }
        }
        if err_open + bad_lines > 0 {
            log::debug!("[kimi] 采集异常统计：文件打开/读取失败 {err_open}，坏行 {bad_lines}");
        }
        let mut rows: Vec<UsageRow> = dedup.into_values().collect();
        rows.sort_by_key(|r| r.ts);
        Ok(CollectOutput::default().with_rows(rows))
    }
}

/// 当前 Unix 毫秒（hooks 备份文件名用）
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ===== hooks 安装/卸载（增强档；注入器走 config.toml 顶层 [[hooks]] 数组） =====
// 注入形状按官方文档（2026-09-23 源码+文档核实）：仅允许 event/matcher/command/
// timeout 四字段，多余字段直接导致配置加载失败——因此只写这三个。

mod hooks {
    use std::path::{Path, PathBuf};

    use super::{kimi_home, now_ms};
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

    /// 桥脚本注入标记（与 CC/Codex 共用同一份桥脚本源码，argv 区分 agent）
    pub const BRIDGE_MARK: &str = "hook-bridge.js";
    /// 注入事件集（20 事件中状态机可消费的 12 个）：失败类（StopFailure/
    /// PostToolUseFailure）是 Kimi 状态精度的核心价值（04-EXPANSION §2.7）
    pub const HOOK_EVENTS: &[&str] = &[
        "SessionStart",
        "UserPromptSubmit",
        "TurnStarted",
        "PreToolUse",
        "PostToolUse",
        "PostToolUseFailure",
        "Stop",
        "StopFailure",
        "PermissionRequest",
        "Interrupt",
        "Notification",
        "SessionEnd",
    ];

    fn config_path() -> Option<PathBuf> {
        Some(kimi_home()?.join("config.toml"))
    }

    fn bridge_script_path() -> Option<PathBuf> {
        Some(kimi_home()?.join("hooks").join("hook-bridge.js"))
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

    /// 安装：桥脚本写出到 $KIMI_CODE_HOME/hooks/，config.toml 备份后注入 12 事件
    pub fn install_hooks() -> anyhow::Result<usize> {
        let bridge = bridge_script_path().ok_or_else(|| anyhow::anyhow!("无法定位 KIMI_CODE_HOME"))?;
        if let Some(dir) = bridge.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&bridge, super::super::claude_code::BRIDGE_SOURCE)?;
        let settings = config_path().ok_or_else(|| anyhow::anyhow!("无法定位 config.toml"))?;
        if !settings.exists() {
            std::fs::write(&settings, "")?;
        }
        // 第二参数是 Agent 标识：桥脚本据此写入 events/kimi-code.jsonl
        let cmd = format!(
            "node \"{}\" kimi-code",
            bridge.to_string_lossy().replace('\\', "/")
        );
        let injected = inject_into_config(&settings, &cmd)?;
        log::info!("[kimi] hooks 安装完成：注入 {injected} 个事件，桥脚本 {}", bridge.display());
        Ok(injected)
    }

    /// 卸载：移除全部自家注入条目；返回移除条数（桥脚本文件保留）
    pub fn uninstall_hooks() -> anyhow::Result<usize> {
        let settings = config_path().ok_or_else(|| anyhow::anyhow!("无法定位 config.toml"))?;
        if !settings.exists() {
            return Ok(0);
        }
        let removed = uninstall_from_config(&settings)?;
        log::info!("[kimi] hooks 卸载完成：移除 {removed} 个注入条目");
        Ok(removed)
    }

    /// 原子写 + 占用重试（与 CC/Codex 注入器同策略；toml_edit 保留用户注释与格式）
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

    /// 单条 [[hooks]] 是否为自家条目（标记在 command 里）
    fn entry_is_ours(entry: &Table) -> bool {
        entry
            .get("command")
            .and_then(|c| c.as_str())
            .map(|c| c.contains(BRIDGE_MARK))
            .unwrap_or(false)
    }

    /// config.toml 注入核心（独立函数便于用临时文件单测）
    fn inject_into_config(path: &Path, command: &str) -> anyhow::Result<usize> {
        let raw = std::fs::read_to_string(path)?;
        let bak = path.with_extension(format!("toml.bak-at-{}", now_ms()));
        std::fs::write(&bak, &raw)?;
        let mut doc = raw
            .parse::<DocumentMut>()
            .map_err(|e| anyhow::anyhow!("config.toml 解析失败（不碰用户配置）：{e}"))?;
        let root = doc.as_table_mut();
        // 顶层 hooks 键 = [[hooks]] 数组表；存在但非数组（用户写成 [hooks] 普通表）→ 保守跳过
        if root.get("hooks").is_none() {
            root.insert("hooks", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let Some(aot) = root.get_mut("hooks").and_then(|i| i.as_array_of_tables_mut()) else {
            anyhow::bail!("config.toml 的 hooks 键不是数组表结构：保守跳过，不碰用户配置");
        };
        let mut injected = 0usize;
        for ev in HOOK_EVENTS {
            // 防重复：同事件已有自家条目即跳过（用户可对同事件自配其他 hook，共存）
            let already = aot
                .iter()
                .any(|t| entry_is_ours(t) && t.get("event").and_then(|e| e.as_str()) == Some(*ev));
            if already {
                continue;
            }
            let mut entry = Table::new();
            entry.insert("event", value(*ev));
            entry.insert("command", value(command));
            entry.insert("timeout", value(10i64));
            aot.push(entry);
            injected += 1;
        }
        if injected > 0 {
            atomic_write_retry(path, &doc.to_string())?;
        }
        Ok(injected)
    }

    /// config.toml 卸载核心：移除全部自家条目，用户条目原样保留
    fn uninstall_from_config(path: &Path) -> anyhow::Result<usize> {
        let raw = std::fs::read_to_string(path)?;
        let mut doc = raw
            .parse::<DocumentMut>()
            .map_err(|e| anyhow::anyhow!("config.toml 解析失败：{e}"))?;
        let Some(aot) = doc
            .get_mut("hooks")
            .and_then(|i| i.as_array_of_tables_mut())
        else {
            return Ok(0);
        };
        let before = aot.len();
        aot.retain(|t| !entry_is_ours(t));
        let removed = before - aot.len();
        if aot.is_empty() {
            doc.as_table_mut().remove("hooks");
        }
        atomic_write_retry(path, &doc.to_string())?;
        Ok(removed)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn tmp_config(tag: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("at-kimi-hooks-{}-{}", tag, std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            dir.join("config.toml")
        }

        /// 注入/卸载往返：用户注释与自配 hook 不受损，自家条目完整还原
        #[test]
        fn test_hooks_install_uninstall_roundtrip() {
            let cfg = tmp_config("rt");
            std::fs::write(&cfg, concat!(
                "# 用户注释必须保留\n",
                "model = \"kimi-k2\"\n",
                "\n",
                "[[hooks]]\n",
                "event = \"PreToolUse\"\n",
                "command = \"python user_hook.py\"\n",
            )).unwrap();

            let cmd = "node \"C:/x/.kimi-code/hooks/hook-bridge.js\" kimi-code";
            let n = inject_into_config(&cfg, cmd).unwrap();
            assert_eq!(n, 12, "注入 12 个事件");
            let s1 = std::fs::read_to_string(&cfg).unwrap();
            assert!(s1.contains("# 用户注释必须保留"), "用户注释保留");
            assert!(s1.contains("python user_hook.py"), "用户自配 hook 保留");
            assert!(s1.contains("event = \"StopFailure\""), "失败类事件已注入");
            assert!(!s1.contains("matcher"), "只写 event/command/timeout 三字段");

            // 防重复
            let n2 = inject_into_config(&cfg, cmd).unwrap();
            assert_eq!(n2, 0);

            // 卸载
            let removed = uninstall_from_config(&cfg).unwrap();
            assert_eq!(removed, 12);
            let s2 = std::fs::read_to_string(&cfg).unwrap();
            assert!(s2.contains("python user_hook.py"), "用户 hook 保留");
            assert!(!s2.contains("hook-bridge.js"), "自家条目全清");
            assert!(s2.contains("model = \"kimi-k2\""));
            let _ = std::fs::remove_file(&cfg);
        }

        /// 空文件（Kimi 未装过）注入：生成合法 TOML 且卸载后 hooks 段整体消失
        #[test]
        fn test_hooks_install_on_empty_config() {
            let cfg = tmp_config("empty");
            std::fs::write(&cfg, "").unwrap();
            // 命令必须含 BRIDGE_MARK：卸载按标记识别自家条目
            let n = inject_into_config(&cfg, "node \"C:/x/.kimi-code/hooks/hook-bridge.js\" kimi-code")
                .unwrap();
            assert_eq!(n, 12);
            let parsed: DocumentMut = std::fs::read_to_string(&cfg).unwrap().parse().unwrap();
            assert!(parsed.get("hooks").is_some());
            let removed = uninstall_from_config(&cfg).unwrap();
            assert_eq!(removed, 12);
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
        let dir = std::env::temp_dir().join(format!("at-kimi-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 写一个合成会话：sessions/<wd>/<sid>/{state.json, agents/main/wire.jsonl}
    fn make_session(root: &Path, sid: &str, title: &str, cwd: &str, wire_lines: &[String]) -> PathBuf {
        let session = root.join("wd_demo-project_ab12cdef7890").join(sid);
        let agents = session.join("agents").join("main");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            session.join("state.json"),
            format!(
                r#"{{"id":"{sid}","version":2,"title":"{title}","createdAt":1789965600000,"updatedAt":1789965660000,"cwd":"{cwd}"}}"#
            ),
        )
        .unwrap();
        std::fs::write(agents.join("wire.jsonl"), wire_lines.join("\n")).unwrap();
        session
    }

    /// 合成 wire 端到端：usage.record 解析（camelCase 四字段）+ state.json 元数据 +
    /// 幂等指纹 + 水位过滤 + 非 usage 行忽略
    #[test]
    fn test_collect_wire() {
        let dir = tmp_dir("collect");
        let sid = "sess-kimi-0001";
        let wire_lines = vec![
            // 首行 metadata：忽略
            r#"{"type":"metadata","protocol_version":"1","created_at":1789965600000}"#.to_string(),
            // usage.record：四项用量（camelCase）
            r#"{"type":"usage.record","time":1789965601000,"agentId":"main","model":"kimi-k2-0905-preview","usage":{"inputOther":120,"output":45,"inputCacheRead":300,"inputCacheCreation":8},"usageScope":"session"}"#.to_string(),
            // 第二次调用
            r#"{"type":"usage.record","time":1789965620000,"agentId":"main","model":"kimi-k2-0905-preview","usage":{"inputOther":50,"output":30,"inputCacheRead":420,"inputCacheCreation":0}}"#.to_string(),
            // 无关行
            r#"{"type":"turn.ended","time":1789965621000,"reason":"completed","durationMs":21000}"#.to_string(),
        ];
        make_session(&dir, sid, "修复登录页", "F:/Demo/proj", &wire_lines);

        let ad = KimiCodeAdapter::with_root(dir.clone());
        let sessions = ad.scan_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, format!("kimi-code:{sid}"));
        assert_eq!(sessions[0].title.as_deref(), Some("修复登录页"));
        assert_eq!(sessions[0].project_dir.as_deref(), Some("F:/Demo/proj"));

        let out = ad.collect_usage(0).unwrap();
        assert_eq!(out.rows.len(), 2, "两条 usage.record + 忽略其他行");
        let first = out.rows.iter().find(|r| r.ts == 1_789_965_601_000).unwrap();
        assert_eq!(first.input_tokens, Some(120));
        assert_eq!(first.output_tokens, Some(45));
        assert_eq!(first.cache_read_tokens, Some(300));
        assert_eq!(first.cache_creation_tokens, Some(8));
        assert_eq!(first.model, "kimi-k2-0905-preview");
        assert_eq!(first.provider.as_deref(), Some("moonshot"));
        assert!(first.source_id.as_deref().unwrap().starts_with("kr:main:1789965601000:"));

        // 幂等：同批重复解析（重建适配器模拟重启全量回溯）→ 指纹一致，行数不变
        let ad2 = KimiCodeAdapter::with_root(dir.clone());
        let out2 = ad2.collect_usage(0).unwrap();
        let ids1: std::collections::HashSet<_> = out.rows.iter().filter_map(|r| r.source_id.clone()).collect();
        let ids2: std::collections::HashSet<_> = out2.rows.iter().filter_map(|r| r.source_id.clone()).collect();
        assert_eq!(ids1, ids2, "内容指纹跨实例稳定（自库幂等键的前提）");

        // 水位：远未来 → 空
        assert!(ad.collect_usage(1_800_000_000_000_000).unwrap().rows.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 集成：本机真实 Kimi Code 数据（手动：cargo test -- --ignored test_real_kimi）。
    /// 待所有者装机后执行：scan/collect 形状 + 水位增量 + 官方口径对账 +
    /// usageScope 语义验证（每请求一行假设是否成立，见 01-RESEARCH §11）
    #[test]
    #[ignore]
    fn test_real_kimi() {
        let ad = KimiCodeAdapter::new();
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 Kimi Code 会话（未装机时应跳过本测试）");
        assert!(sessions.iter().all(|s| s.id.starts_with("kimi-code:")));
        let usage = ad.collect_usage(0).unwrap().rows;
        assert!(!usage.is_empty(), "本机应有历史用量");
        let max_ts = usage.iter().map(|u| u.ts).max().unwrap();
        let second = ad.collect_usage(max_ts).unwrap().rows;
        assert!(second.len() <= 5, "水位增量应接近空，实际 {} 行", second.len());
        println!("Kimi 会话 {} 个，用量 {} 行", sessions.len(), usage.len());
        let sum = |f: fn(&UsageRow) -> Option<i64>| usage.iter().filter_map(f).sum::<i64>();
        println!("input:  {}", sum(|r| r.input_tokens));
        println!("output: {}", sum(|r| r.output_tokens));
        println!("cache_read: {}", sum(|r| r.cache_read_tokens));
        println!("cache_creation: {}", sum(|r| r.cache_creation_tokens));
    }
}
