//! Aider 适配器（M2-19）：降级档——旁路读取散落在各 git 仓库根的
//! `.aider.chat.history.md`（会话 markdown）。勘察（2026-10-09 源码级核实：
//! Aider-AI/aider main 分支 io.py/base_coder.py/args.py，网络调研无装机）：
//!   ① 用量数据面缺失（设计已知）：token/cost 只进终端展示（base_coder.py
//!     usage_report），历史文件只写对话文本；--llm-history-file 默认关闭——
//!     **collect_usage 恒空**（UsageRow 语义=一次模型调用，无数据可采），
//!     岛端状态与会话活跃性走 scan 的标记行时间，额度/消耗显示按「无本地
//!     记账」降级（红线④渐进降级：会话观测是增强不是依赖）；
//!   ② 历史文件：git root（无 git 则 cwd）/.aider.chat.history.md，append-only
//!     多会话共存；会话边界 = `\n# aider chat started at YYYY-MM-DD HH:MM:SS\n`
//!     （io.py:336，datetime.now() 本地时区）——会话切分与时间戳同源；用户
//!     输入以 "> " 前缀行落盘（首条即会话标题素材）；
//!   ③ 发现边界（如实声明）：aider 历史散落在任意仓库根、无全局家目录——
//!     探测根取家目录整体递归（GlobWalker 目录 mtime 剪枝缓存兜性能；自研
//!     seg_match 对点开头文件名无特判，字面量模式精确匹配）；
//!     --chat-history-file 自定义文件名不认（装机需求再议）；
//!   ④ 会话 id："aider:{文件全路径}:{起始毫秒}"——同文件多会话以标记行时间
//!     消歧（同秒双开撞 id 极罕见，容忍）；
//!   ⑤ 无 hooks 注入（aider 无 hooks 体系）；进程关键词 "aider"（pip 安装
//!     的 aider/aider.exe；bat shim 命令行兼查）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::engine::{mtime_ms, now_ms, GlobWalker, HotSignal, ProcessMatch, RANGE_CUTOFF_MS};
use super::{AgentAdapter, CollectOutput, SessionInfo};

/// 会话标题最大长度（字符数；岛端与报表标题展示的统一口径）
const TITLE_MAX_CHARS: usize = 50;

pub struct AiderAdapter {
    /// 探测根（家目录：历史文件散落各仓库，从家目录整体发现，模块注释③）
    root: PathBuf,
    /// 目录枚举树器（目录 mtime 剪枝缓存，CC 同款）
    walker: Mutex<GlobWalker>,
    /// 历史文件解析缓存：路径 → (mtime_ms, 会话列表)。append-only 文件 mtime
    /// 不变时零重读（长历史文件数十万行时省每 tick 全文解析）
    file_cache: Mutex<HashMap<PathBuf, (i64, Vec<SessionInfo>)>>,
}

impl AiderAdapter {
    /// 生产档：家目录为探测根
    pub fn new() -> Self {
        let root = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_default();
        Self::with_root(root)
    }

    /// 指定探测根构造（单测注入临时目录用）
    pub(crate) fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            walker: Mutex::new(GlobWalker::default()),
            file_cache: Mutex::new(HashMap::new()),
        }
    }

    fn lock_walker(&self) -> MutexGuard<'_, GlobWalker> {
        self.walker.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_cache(&self) -> MutexGuard<'_, HashMap<PathBuf, (i64, Vec<SessionInfo>)>> {
        self.file_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for AiderAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// 本地时区时间串（aider datetime.now() 的 %Y-%m-%d %H:%M:%S）→ Unix 毫秒。
/// DST 歧义时刻 single() 为空时取 earliest 兜底；解析失败返回 None（该会话跳过）
fn parse_local_ms(s: &str) -> Option<i64> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let local = chrono::Local.from_local_datetime(&naive);
    local
        .single()
        .or_else(|| local.earliest())
        .map(|d| d.timestamp_millis())
}

/// 解析单个历史文件的全部会话块（模块注释②④）：
/// 标记行 `# aider chat started at <时间>` 开启新会话；块内首条 "> " 行取标题
/// （截 TITLE_MAX_CHARS 字符，不切断多字节字符）。时间解析失败的块跳过。
fn parse_sessions(path: &std::path::Path, mtime: i64, text: &str) -> Vec<SessionInfo> {
    const MARK: &str = "# aider chat started at ";
    let path_str = path.to_string_lossy().to_string();
    let mut out: Vec<SessionInfo> = vec![];
    // 当前会话状态：(标记行时间毫秒，标题是否已取)
    let mut cur: Option<(i64, Option<String>)> = None;
    let flush =
        |cur: &mut Option<(i64, Option<String>)>, out: &mut Vec<SessionInfo>| {
            if let Some((start_ms, title)) = cur.take() {
                out.push(SessionInfo {
                    id: format!("aider:{path_str}:{start_ms}"),
                    agent: "aider".into(),
                    provider: None,
                    model: None,
                    project_dir: path
                        .parent()
                        .map(|p| p.to_string_lossy().to_string()),
                    title,
                    first_seen_at: start_ms,
                    last_seen_at: start_ms,
                    last_usage_at: Some(start_ms),
                });
            }
        };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(MARK) {
            // 新会话块：先落盘上一块
            flush(&mut cur, &mut out);
            if let Some(start_ms) = parse_local_ms(rest) {
                cur = Some((start_ms, None));
            }
            continue;
        }
        // 会话内首条用户输入行（"> " 前缀）作为标题
        if let Some((_, title @ None)) = cur.as_mut() {
            if let Some(input) = line.strip_prefix("> ") {
                let trimmed = input.trim();
                if !trimmed.is_empty() {
                    let mut t = trimmed.to_string();
                    if t.chars().count() > TITLE_MAX_CHARS {
                        t = t.chars().take(TITLE_MAX_CHARS).collect();
                    }
                    *title = Some(t);
                }
            }
        }
    }
    flush(&mut cur, &mut out);
    // mtime 仅兜底文件级过滤，不影响会话时间轴（last_seen 用标记行时间）
    let _ = mtime;
    out
}

impl AgentAdapter for AiderAdapter {
    fn id(&self) -> &'static str {
        "aider"
    }

    /// 快轮信号：家目录浅层扫描（ext 限定 .md 有噪音（README 等变化也触发），
    /// 但信号只唤醒全量 tick、scan 内部只认 .aider.chat.history.md，幂等无害）
    fn hot_signals(&self) -> Vec<HotSignal> {
        vec![HotSignal::DirScan {
            root: self.root.clone(),
            ext: Some(".md"),
            depth: 6,
            max_files: 200,
        }]
    }

    /// 进程匹配：aider/aider.exe（pip 安装；bat shim 命令行兼查）
    fn process_match(&self) -> Option<ProcessMatch> {
        Some(ProcessMatch {
            name_keywords: &["aider"],
            cmd_keywords: &["aider"],
            cmd_excludes: &["agenttrackerisland"],
        })
    }

    /// 会话发现：枚举全部历史文件 → mtime 缓存解析 → 90 天窗口内会话列表。
    /// collect_usage 恒空（模块注释①），scan 的 last_usage_at 是唯一活动信号
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        // walker 需可变（内部剪枝缓存），自持锁
        let files = {
            let mut w = self.lock_walker();
            w.list(&self.root, "**/.aider.chat.history.md")
        };
        let cutoff = now_ms() - RANGE_CUTOFF_MS;
        let mut out = vec![];
        for f in files {
            let mtime = mtime_ms(&f);
            if mtime < cutoff {
                continue; // 文件久未动，必无新会话（窗口外）
            }
            // 缓存命中：mtime 未变直接复用上轮解析结果
            let cached = {
                let map = self.lock_cache();
                map.get(&f).filter(|(m0, _)| *m0 == mtime).map(|(_, v)| v.clone())
            };
            let sessions = match cached {
                Some(v) => v,
                None => {
                    let text = match std::fs::read_to_string(&f) {
                        Ok(t) => t,
                        Err(_) => continue, // 文件被占用/权限抖动：本轮跳过下轮再试
                    };
                    let parsed = parse_sessions(&f, mtime, &text);
                    self.lock_cache().insert(f.clone(), (mtime, parsed.clone()));
                    parsed
                }
            };
            // 会话级窗口过滤：起始时间在最近 90 天内的才纳入（老会话不入岛）
            out.extend(sessions.into_iter().filter(|s| s.last_seen_at > cutoff));
        }
        // 最近活跃在前，截断 100（与 CC 家族同口径）
        out.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
        if out.len() > 100 {
            log::debug!("[aider] 会话 {} 个，截断保留最近 100", out.len());
        }
        out.truncate(100);
        Ok(out)
    }

    /// 用量增量采集：恒空——aider 无任何本地 token/模型记账（模块注释①）。
    /// 返回 Ok(default) 而非 Err：Err 会被上层当作采集故障，恒空是数据源事实
    fn collect_usage(&self, _watermark_ts: i64) -> anyhow::Result<CollectOutput> {
        Ok(CollectOutput::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录灌两个历史文件（多会话块/标题行/坏时间块混合）
    fn seed(dir: &std::path::Path) {
        // 项目 A：两个会话块
        let proj_a = dir.join("F--proj-a");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::write(
            proj_a.join(".aider.chat.history.md"),
            "# 旧内容头部\n\
             \n# aider chat started at 2026-10-08 09:00:00\n\
             \n> 帮我修复登录页的空指针异常\n\
             \n好的，我来分析登录页代码……\n\
             \n# aider chat started at 2026-10-09 14:30:00\n\
             \n> 重构采集引擎为声明式模型，这是一条超长标题需要被截断处理的用户输入示例文本请继续补充到五十个字符以上以便验证截断逻辑生效\n\
             \n已开始重构……\n",
        )
        .unwrap();
        // 项目 B：一个会话（时间行损坏 → 该块跳过）
        let proj_b = dir.join("F--proj-b");
        std::fs::create_dir_all(&proj_b).unwrap();
        std::fs::write(
            proj_b.join(".aider.chat.history.md"),
            "# aider chat started at not-a-time\n\
             > 不会出现的标题\n\
             \n# aider chat started at 2026-10-09 20:00:00\n\
             \n> 给报表页加个导出按钮\n",
        )
        .unwrap();
        // 无关文件：不应被发现
        std::fs::write(dir.join("README.md"), "# not aider").unwrap();
    }

    #[test]
    fn test_scan_sessions() {
        let dir = std::env::temp_dir().join(format!("at-aider-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        seed(&dir);
        let ad = AiderAdapter::with_root(dir.clone());

        let sessions = ad.scan_sessions().unwrap();
        // proj_a 两块 + proj_b 一块（坏时间块跳过）；README.md 不匹配
        assert_eq!(sessions.len(), 3, "会话块切分正确（坏时间块跳过）");
        // 最近活跃在前
        assert!(sessions[0].last_seen_at >= sessions[1].last_seen_at);
        // id 含路径与起始时间；agent 前缀正确
        assert!(sessions.iter().all(|s| s.agent == "aider"));
        assert!(sessions.iter().all(|s| s.id.starts_with("aider:")));
        // 标题 = 首条 "> " 行截断（超长标题截 TITLE_MAX_CHARS）
        let long_title = sessions
            .iter()
            .find(|s| s.title.as_deref().unwrap_or("").starts_with("重构采集引擎"))
            .expect("超长标题会话存在");
        assert_eq!(long_title.title.as_deref().map(str::chars).map(|c| c.count()), Some(TITLE_MAX_CHARS));
        // 项目目录 = 历史文件父目录
        let proj_b = dir.join("F--proj-b");
        let export = sessions.iter().find(|s| s.title.as_deref() == Some("给报表页加个导出按钮")).unwrap();
        assert_eq!(export.project_dir, Some(proj_b.to_string_lossy().to_string()));
        // collect_usage 恒空（降级档语义）
        assert!(ad.collect_usage(0).unwrap().rows.is_empty());
        // 进程/信号声明
        assert_eq!(ad.process_match().unwrap().name_keywords, &["aider"]);
        assert!(matches!(ad.hot_signals()[0], HotSignal::DirScan { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// mtime 缓存：文件未变时第二次 scan 复用解析结果（行为等价，仅性能路径）
    #[test]
    fn test_file_cache_reuse() {
        let dir = std::env::temp_dir().join(format!("at-aider-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        seed(&dir);
        let ad = AiderAdapter::with_root(dir.clone());
        let s1 = ad.scan_sessions().unwrap();
        let s2 = ad.scan_sessions().unwrap();
        assert_eq!(s1.len(), s2.len());
        assert_eq!(s1[0].id, s2[0].id, "同文件同会话 id 稳定");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 本地时区解析：秒级精度串转毫秒（与 chrono Local 语义对齐）
    #[test]
    fn test_parse_local_ms() {
        let ms = parse_local_ms("2026-10-09 12:00:00").unwrap();
        assert!(ms > 1_700_000_000_000 && ms % 1000 == 0, "秒级串应为整秒毫秒：{ms}");
        assert!(parse_local_ms("not-a-time").is_none());
        assert!(parse_local_ms("").is_none());
    }

    /// 集成：本机真实 aider 历史（未装/未用过跳过不了——ignore 手动跑）
    /// 手动运行：cargo test -- --ignored
    #[test]
    #[ignore]
    fn test_real_aider_collect() {
        let ad = AiderAdapter::new();
        let sessions = ad.scan_sessions().unwrap();
        assert!(!sessions.is_empty(), "本机应有 aider 会话历史");
        assert!(sessions.iter().all(|s| s.id.starts_with("aider:")));
        assert!(ad.collect_usage(0).unwrap().rows.is_empty(), "降级档恒空");
    }
}
