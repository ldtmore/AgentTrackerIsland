//! 通用采集引擎：文件枚举/增量读/SQLite 只读打开/扫描节流/快轮信号等与 Agent 无关的机制层。
//! 设计约束（04-EXPANSION §2.1）：适配器只提供声明与解析（策略层），
//! 本模块不出现任何 per-agent 分支；机制行为与既有 CC/ZCode 采集逐条对齐（M2-3 迁移）。

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// 单文件增量回退字节量：与 service 层 60s 水位余量配对，覆盖「行写入顺序
/// 与时间戳乱序」的边缘；回读的旧行靠调用内去重 + 自库幂等键兜底，不会重复
/// （自 claude_code.rs 原常量迁移，语义不变）
pub const BACKTRACK_BYTES: u64 = 64 * 1024;

// ===== Glob 目录枚举（支持 ** 递归 + 目录 mtime 剪枝缓存） =====

/// 单段模式：`**`（任意层级，含零层）或含 `*` 的普通段（如 `*.jsonl`、字面量）
#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Recursive,
    Pat(String),
}

/// Glob 走树器：持有「目录 mtime → 目录条目名」缓存——目录 mtime 仅在条目增删时
/// 变化，未变则复用缓存免重读（剪枝），文件内容变化不影响本缓存（改由 per-file
/// mtime 判定）。有状态，调用方持有实例（适配器内 Mutex 包裹）
#[derive(Default)]
pub struct GlobWalker {
    dir_cache: HashMap<PathBuf, (i64, Vec<(String, bool)>)>, // （目录 mtime， （名称， 是否目录））
}

impl GlobWalker {
    /// 以 root 为根按 pattern 枚举文件。pattern 相对 root，`/` 分段，
    /// 支持 `**` 递归与 `*` 通配（如 `**/*.jsonl`）。枚举失败（根不存在等）返回空。
    pub fn list(&mut self, root: &Path, pattern: &str) -> Vec<PathBuf> {
        let segs: Vec<Seg> = pattern
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| if s == "**" { Seg::Recursive } else { Seg::Pat(s.to_string()) })
            .collect();
        if segs.is_empty() {
            return vec![];
        }
        let mut out = vec![];
        self.walk(root, &segs, 0, &mut out);
        // 保险去重：含多个 `**` 的复杂模式理论上可经不同消费路径命中同一文件
        out.sort();
        out.dedup();
        out
    }

    /// 递归匹配：segs 首段为 `**` 时同时尝试「消费一层」与「原地零宽」两种走向；
    /// 仅末段允许命中文件，中间段只走目录
    fn walk(&mut self, dir: &Path, segs: &[Seg], depth: u8, out: &mut Vec<PathBuf>) {
        if depth > 24 {
            return; // 防御异常深的目录树/`**` 零宽自组合，硬上限
        }
        let Some((seg, rest)) = segs.split_first() else { return };
        for (name, is_dir) in self.entries(dir) {
            let child = dir.join(&name);
            match seg {
                Seg::Recursive => {
                    if is_dir {
                        // 递归段：进入子目录（层级 ≥1）与原地（零层）两条路都走
                        self.walk(&child, segs, depth + 1, out);
                    }
                }
                Seg::Pat(p) => {
                    if seg_match(p, &name) {
                        if rest.is_empty() {
                            if !is_dir {
                                out.push(child);
                            }
                        } else if is_dir {
                            self.walk(&child, rest, depth + 1, out);
                        }
                    }
                }
            }
        }
        // `**` 的零宽走向：在当前目录继续消费后续段（放在条目循环后，语义等价且免重入）
        if matches!(seg, Seg::Recursive) && !rest.is_empty() {
            self.walk(dir, rest, depth + 1, out);
        }
    }

    /// 目录条目（带 mtime 剪枝缓存）。M2-UX-1 加固：读取失败（目录被占用/权限
    /// 抖动等瞬态故障）**不缓存空结果**——缓存空列表会在目录 mtime 不变期间
    /// 造成永久失明（自愈能力弱于改造前的每轮直读），失败只允许影响当轮
    fn entries(&mut self, dir: &Path) -> Vec<(String, bool)> {
        let mtime = mtime_ms(&dir.to_path_buf());
        if mtime == 0 {
            self.dir_cache.remove(dir);
            return vec![];
        }
        if let Some((m0, cached)) = self.dir_cache.get(dir) {
            if *m0 == mtime {
                return cached.clone();
            }
        }
        let mut list = vec![];
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.filter_map(|x| x.ok()) {
                let name = e.file_name().to_string_lossy().into_owned();
                // is_dir 失败按非目录处理（符号链接异常等极端场景，宁可漏列不 panic）
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                list.push((name, is_dir));
            }
            self.dir_cache.insert(dir.to_path_buf(), (mtime, list.clone()));
        } else {
            self.dir_cache.remove(dir);
        }
        list
    }
}

/// 单段通配匹配：仅支持 `*`（段内任意），字面量精确比较；`*` 前后缀拆分比较
fn seg_match(pat: &str, name: &str) -> bool {
    match pat.split_once('*') {
        None => pat == name,
        Some((prefix, suffix)) => {
            // 多个 `*` 退化为「首段前缀 + 末段后缀」匹配：对本用途（扩展名过滤）足够
            let suffix = suffix.rsplit('*').next().unwrap_or(suffix);
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
    }
}

// ===== 单文件增量读（自 claude_code.rs collect_usage 迁移，行为逐条对齐） =====

/// 一次增量读的产出：start>0 表示起点落在回退区（可能半行，调用方用
/// body_after_partial 跳残行）；text 为 [start, 文件尾) 的内容
pub struct FileDelta {
    pub start: u64,
    pub text: String,
}

/// per-file 增量游标（路径 → （上次 mtime_ms， 已消费字节偏移））
#[derive(Default)]
pub struct IncrementalFileReader {
    offsets: HashMap<PathBuf, (i64, u64)>,
}

impl IncrementalFileReader {
    /// 判定并读取一个文件的新增内容。
    /// 返回 Ok(None)：mtime 未变（游标保留，必无新字节）；
    /// Err：元数据/打开/读失败（文件被占用等），调用方计数留痕后跳过本轮。
    /// 首见或文件被重建（比旧偏移小）→ 归零全读（与原实现一致）
    pub fn changed(&mut self, path: &Path) -> anyhow::Result<Option<FileDelta>> {
        let total = fs::metadata(path)?.len();
        let mtime = mtime_ms(&path.to_path_buf());
        let start = match self.offsets.get(path) {
            Some((m0, _)) if *m0 == mtime => return Ok(None),
            Some((_, off)) if *off <= total => off.saturating_sub(BACKTRACK_BYTES),
            _ => 0,
        };
        let mut f = fs::File::open(path)?;
        let mut buf = Vec::new();
        if start > 0 {
            f.seek(SeekFrom::Start(start))?;
        }
        f.read_to_end(&mut buf)?;
        // 记录新游标 = 文件当前全长度（每轮都读到尾）
        self.offsets.insert(path.to_path_buf(), (mtime, total));
        Ok(Some(FileDelta { start, text: String::from_utf8_lossy(&buf).into_owned() }))
    }
}

/// 起点落在半行中间（回退导致）：跳过该残行，从下一个换行起解析；
/// 无完整新行返回 None（与原实现一致）
pub fn body_after_partial<'a>(text: &'a str, start: u64) -> Option<&'a str> {
    if start == 0 {
        return Some(text);
    }
    match text.find('\n') {
        Some(i) => Some(&text[i + 1..]),
        None => None,
    }
}

// ===== 扫描节流（SQLite 档自律预算，04-EXPANSION §2.3.2） =====

/// 扫描预算：距上次放行不足 min_interval 时拒绝本次扫描，调用方返回缓存结果。
/// 有状态，适配器内持有（scan 与 collect 各一份，避免相互吃掉预算）
#[derive(Debug)]
pub struct ScanBudget {
    min_interval_ms: u64,
    last: Option<Instant>,
}

impl ScanBudget {
    pub fn new(min_interval_ms: u64) -> Self {
        Self { min_interval_ms, last: None }
    }

    /// 首次恒放行；此后距上次放行不足预算则拒绝
    pub fn ready(&mut self) -> bool {
        match self.last {
            None => {
                self.last = Some(Instant::now());
                true
            }
            Some(t) => {
                if t.elapsed().as_millis() as u64 >= self.min_interval_ms {
                    self.last = Some(Instant::now());
                    true
                } else {
                    false
                }
            }
        }
    }
}

// ===== SQLite 只读打开（自 zcode.rs 迁移的公共助手） =====

/// 只读打开（WAL 并发读安全；Agent 运行与否均可读）。库不存在即报错，
/// 由调用方按「未安装=静默降级」处理
pub fn open_sqlite_readonly(path: &Path) -> anyhow::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Ok(conn)
}

// ===== 快轮信号（调度器 1~2s 高频探测的目标；值变化才唤醒全量 tick） =====

/// 路径提供器：闭包返回目标路径（None=目标当前不存在，如 hooks 未安装/
/// 跨零点日志尚未创建），返回 Option 以支持「从无到有」本身作为信号
pub type PathProvider = std::sync::Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// 快轮信号声明：适配器通过 `AgentAdapter::hot_signals` 提供
pub enum HotSignal {
    /// 单文件探测（hook 事件文件、当日日志等）
    File(PathProvider),
    /// 目录探测：限深枚举取 max(mtime) 与文件数（未装 hooks 的文件型 Agent 的转录树）。
    /// depth 为相对 root 的下潜层数上限：0=仅 root 直属文件，1=可进入一层子目录，以此类推
    DirScan {
        root: PathBuf,
        /// 扩展名过滤（含点，如 ".jsonl"）；None=不过滤
        ext: Option<&'static str>,
        /// 相对 root 的枚举深度上限（1=仅 root 直属文件）
        depth: u8,
        max_files: usize,
    },
}

/// 信号采样值（PartialEq 判变化；不同形态间恒不等）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignalValue {
    File { len: u64, mtime: i64 },
    Dir { count: usize, max_mtime: i64 },
    /// 目标存在但取不到长度/时间等异常：视作占位，避免与 None（不存在）混淆抖动
    Unreadable,
}

impl HotSignal {
    /// 采样当前值；None = 目标不存在（首现时 None→Some 即变化）
    pub fn sample(&self) -> Option<SignalValue> {
        match self {
            HotSignal::File(provider) => {
                let p = provider()?;
                let meta = fs::metadata(&p).ok()?;
                let mtime = mtime_ms(&p);
                if meta.is_file() && mtime > 0 {
                    Some(SignalValue::File { len: meta.len(), mtime })
                } else {
                    Some(SignalValue::Unreadable)
                }
            }
            HotSignal::DirScan { root, ext, depth, max_files } => {
                if !root.is_dir() {
                    return None;
                }
                let mut count = 0usize;
                let mut max_mtime = 0i64;
                let mut stack = vec![(root.clone(), 0u8)];
                while let Some((dir, d)) = stack.pop() {
                    let Ok(rd) = fs::read_dir(&dir) else { continue };
                    for e in rd.filter_map(|x| x.ok()) {
                        if count >= *max_files {
                            return Some(SignalValue::Dir { count, max_mtime });
                        }
                        let Ok(ft) = e.file_type() else { continue };
                        let p = e.path();
                        if ft.is_dir() {
                            if d + 1 <= *depth {
                                stack.push((p, d + 1));
                            }
                        } else if ext.map(|x| p.extension().and_then(|e| e.to_str()) == Some(x.trim_start_matches('.')))
                            .unwrap_or(true)
                        {
                            count += 1;
                            max_mtime = max_mtime.max(mtime_ms(&p));
                        }
                    }
                }
                Some(SignalValue::Dir { count, max_mtime })
            }
        }
    }
}

/// 文件 mtime（Unix 毫秒）；取不到返回 0（自 claude_code.rs 迁移）
pub fn mtime_ms(p: &Path) -> i64 {
    p.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// ISO 8601（如 2026-09-16T06:46:19.159Z）→ Unix 毫秒；解析失败返回 None。
/// Claude Code 与 Codex 的转录行时间戳同为 RFC3339 风格字符串（自
/// claude_code.rs 提升为公共助手，M2-6 两家复用）
pub fn iso_to_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// 进程匹配规则（进程枚举的适配器私有知识，M2-4 声明化）：
/// 进程名或命令行任一关键词命中，且命令行不含任一排除词，即判该 Agent 存活
#[derive(Debug, Clone)]
pub struct ProcessMatch {
    pub name_keywords: &'static [&'static str],
    pub cmd_keywords: &'static [&'static str],
    pub cmd_excludes: &'static [&'static str],
}

// ===== hooks JSON 配置注入器（M2-11 自 claude_code.rs 迁入并公共化） =====
// 适用：claude-code / gemini / qwen-code 三家的 settings.json（`hooks` 键为
// 「事件名 → HookDefinition 数组」的 JSON 对象，三家 schema 同构）。
// Codex/Kimi 走 TOML（toml_edit），不在此列。

/// 当前 Unix 毫秒
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 原子写（临时文件 + rename）。目标被占用时（典型：编辑器常驻打开 settings.json）
/// Windows 的 rename 会失败——退避重试三次后放弃并给出可操作的指引（审查 2.1.4）
pub(crate) fn atomic_write_retry(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.at-tmp");
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
        "写入 {} 失败（目标可能被编辑器占用，请关闭正在编辑该文件的程序后重试）：{}",
        path.display(),
        last_err.unwrap()
    ))
}

/// 清理历史备份，只保留最近 keep 份（审查 2.1.4：备份文件名带毫秒时间戳，
/// 字典序即时间序；此前无限累积）
pub(crate) fn prune_backups(path: &Path, keep: usize) {
    let Some(dir) = path.parent() else { return };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return };
    let prefix = format!("{name}.bak-at-");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut baks: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect();
    baks.sort();
    if baks.len() <= keep {
        return;
    }
    let removed = baks.len() - keep;
    for stale in baks.iter().take(removed) {
        let _ = std::fs::remove_file(stale);
    }
    log::info!("hooks 备份清理：移除 {removed} 份历史备份，保留最近 {keep} 份");
}

/// settings.json 注入核心：events 逐事件追加 entry（自家条目按 mark 识别防重复，
/// 用户已有同名事件则追加不覆盖；事件键非数组的保守跳过）。返回注入条数。
/// 原子写 + 带时间戳备份（解析成功后才写备份，绝不弄坏用户配置）
pub(crate) fn inject_json_hooks(
    path: &Path,
    events: &[&str],
    entry: serde_json::Value,
    mark: &str,
) -> anyhow::Result<usize> {
    let raw = std::fs::read_to_string(path)?;
    let mut s: serde_json::Value = serde_json::from_str(&raw)?;
    // 备份（带时间戳，不覆盖历史备份）
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("settings.json");
    let bak = path.with_file_name(format!("{name}.bak-at-{}", now_ms()));
    std::fs::write(&bak, &raw)?;
    // 确保 hooks 对象存在
    if s.get("hooks").and_then(|h| h.as_object()).is_none() {
        s["hooks"] = serde_json::json!({});
    }
    let hooks = s["hooks"].as_object_mut().unwrap();
    let mut injected = 0usize;
    for ev in events {
        let entry_arr = hooks.entry(ev.to_string()).or_insert(serde_json::json!([]));
        if !entry_arr.is_array() {
            continue; // 用户配置了非数组结构：不碰，保守跳过
        }
        let already = entry_arr.as_array().unwrap().iter().any(|g| {
            g["hooks"].as_array().map(|hs| hs.iter().any(|h| {
                h["command"].as_str().map(|c| c.contains(mark)).unwrap_or(false)
            })).unwrap_or(false)
        });
        if already {
            continue;
        }
        entry_arr.as_array_mut().unwrap().push(entry.clone());
        injected += 1;
    }
    atomic_write_retry(path, serde_json::to_string_pretty(&s)?.as_bytes())?;
    prune_backups(path, 5);
    Ok(injected)
}

/// settings.json 卸载核心：移除全部含 mark 的注入条目（含空事件键清理）；
/// 返回移除条数
pub(crate) fn uninstall_json_hooks(path: &Path, mark: &str) -> anyhow::Result<usize> {
    let raw = std::fs::read_to_string(path)?;
    let mut s: serde_json::Value = serde_json::from_str(&raw)?;
    let Some(hooks) = s.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(0);
    };
    let mut removed = 0usize;
    for ev in hooks.keys().cloned().collect::<Vec<_>>() {
        if let Some(arr) = hooks.get_mut(&ev).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|g| {
                !g["hooks"].as_array().map(|hs| hs.iter().any(|h| {
                    h["command"].as_str().map(|c| c.contains(mark)).unwrap_or(false)
                })).unwrap_or(false)
            });
            removed += before - arr.len();
            if arr.is_empty() {
                hooks.remove(&ev);
            }
        }
    }
    if s["hooks"].as_object().map(|o| o.is_empty()).unwrap_or(true) {
        s.as_object_mut().unwrap().remove("hooks");
    }
    atomic_write_retry(path, serde_json::to_string_pretty(&s)?.as_bytes())?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("at-eng-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// GlobWalker：** 递归、* 通配、日期分区目录、目录 mtime 剪枝缓存
    #[test]
    fn test_glob_walker() {
        let dir = tmp_dir("glob");
        // 模拟 Codex 式日期分区：root/2026/09/22/a.jsonl + root/top.jsonl + root/other.txt
        let d1 = dir.join("2026").join("09").join("22");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::write(d1.join("a.jsonl"), "x").unwrap();
        std::fs::write(dir.join("top.jsonl"), "x").unwrap();
        std::fs::write(dir.join("other.txt"), "x").unwrap();

        let mut w = GlobWalker::default();
        let mut hit = w.list(&dir, "**/*.jsonl");
        hit.sort();
        assert_eq!(hit, vec![dir.join("2026").join("09").join("22").join("a.jsonl"), dir.join("top.jsonl")]);

        // 单层目录枚举（ZCode rollout 形态）：depth 语义由调用方用具体 pattern 表达
        let mut w2 = GlobWalker::default();
        let hit2 = w2.list(&dir, "*.jsonl");
        assert_eq!(hit2, vec![dir.join("top.jsonl")], "`*.jsonl` 不应递归进子目录");

        // 目录 mtime 剪枝缓存：mtime 未变时不重读（新增文件必然改父目录 mtime，故先改 mtime 再验证）
        let mut w3 = GlobWalker::default();
        assert_eq!(w3.list(&dir, "*.jsonl").len(), 1);
        // 不改动目录直接再列：命中缓存，结果一致
        assert_eq!(w3.list(&dir, "*.jsonl").len(), 1);
        // 新增文件 → 父目录 mtime 变化 → 缓存失效重读 → 新文件被纳入
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("new.jsonl"), "x").unwrap();
        assert_eq!(w3.list(&dir, "*.jsonl").len(), 2, "目录条目增删后剪枝缓存必须失效");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// IncrementalFileReader：首见全读 → 追加增量（含 64KB 回退）→ 未变化 None → 重建归零
    #[test]
    fn test_incremental_reader() {
        let dir = tmp_dir("incr");
        let f = dir.join("a.jsonl");
        std::fs::write(&f, format!("{}\n", "line1")).unwrap();
        let mut r = IncrementalFileReader::default();
        let d1 = r.changed(&f).unwrap().unwrap();
        assert_eq!(d1.start, 0);
        assert!(d1.text.contains("line1"));
        // 未变化：None
        assert!(r.changed(&f).unwrap().is_none(), "mtime 未变应无新字节");
        // 追加（等待确保 mtime 变化）→ 增量读回；小文件回退被钳到 0（6 字节 - 64KB
        // saturating_sub = 0）＝全量重读，旧行由调用方按水位过滤——与原实现一致
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, format!("{}\n{}\n", "line1", "line2")).unwrap();
        let d2 = r.changed(&f).unwrap().unwrap();
        assert_eq!(d2.start, 0, "小文件回退区被钳到 0");
        assert!(d2.text.contains("line1"));
        assert!(d2.text.contains("line2"));
        // 文件重建（变小）：归零全读
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, "fresh\n").unwrap();
        let d3 = r.changed(&f).unwrap().unwrap();
        assert_eq!(d3.start, 0);
        assert_eq!(d3.text, "fresh\n");
        // 不存在：Err
        assert!(r.changed(&dir.join("nope.jsonl")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// body_after_partial：零宽起点全量返回；半行起点跳残行；无完整行返回 None
    #[test]
    fn test_body_after_partial() {
        assert_eq!(body_after_partial("a\nb", 0), Some("a\nb"));
        assert_eq!(body_after_partial("half\nrest", 7), Some("rest"));
        assert_eq!(body_after_partial("halfno-newline", 7), None);
    }

    /// ScanBudget：首次放行，窗口内拒绝，窗口外放行
    #[test]
    fn test_scan_budget() {
        let mut b = ScanBudget::new(50);
        assert!(b.ready(), "首次恒放行");
        assert!(!b.ready(), "窗口内拒绝");
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(b.ready(), "窗口外放行");
    }

    /// HotSignal 采样：File 形态的从无到有即变化；DirScan 深度与扩展名过滤
    #[test]
    fn test_hot_signal_sample() {
        let dir = tmp_dir("hot");
        let f = dir.join("log.jsonl");
        // File：目标不存在 → None；创建 → Some(File)
        let provider: PathProvider = {
            let p = f.clone();
            std::sync::Arc::new(move || p.exists().then_some(p.clone()))
        };
        let sig = HotSignal::File(provider);
        assert_eq!(sig.sample(), None);
        std::fs::write(&f, "x").unwrap();
        assert!(matches!(sig.sample(), Some(SignalValue::File { .. })));
        // DirScan：depth=1 可下潜一层——root 直属 log.jsonl + 子目录内 deep.jsonl 共 2 个
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("deep.jsonl"), "x").unwrap();
        std::fs::write(dir.join("note.txt"), "x").unwrap();
        let ds = HotSignal::DirScan { root: dir.clone(), ext: Some(".jsonl"), depth: 1, max_files: 100 };
        match ds.sample() {
            Some(SignalValue::Dir { count, .. }) => assert_eq!(count, 2, "root 直属与一层子目录各计 1，txt 被过滤"),
            other => panic!("应为 Dir 采样值：{other:?}"),
        }
        // max_files 截断
        std::fs::write(dir.join("b.jsonl"), "x").unwrap();
        let ds2 = HotSignal::DirScan { root: dir.clone(), ext: Some(".jsonl"), depth: 1, max_files: 1 };
        match ds2.sample() {
            Some(SignalValue::Dir { count, .. }) => assert_eq!(count, 1, "超限即截断"),
            other => panic!("应为 Dir 采样值：{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// seg_match：字面量精确、`*` 前后缀、多 `*` 退化
    #[test]
    fn test_seg_match() {
        assert!(seg_match("*.jsonl", "a.jsonl"));
        assert!(!seg_match("*.jsonl", "a.jsonl.bak"));
        assert!(!seg_match("*.jsonl", "jsonl"));
        assert!(seg_match("exact", "exact"));
        assert!(!seg_match("exact", "exactx"));
        assert!(seg_match("a*b", "axxyb"));
        assert!(!seg_match("a*b", "axxy"));
    }

    /// M2-UX-1 加固回归：read_dir 失败（以文件路径冒充目录）不得缓存空结果，
    /// 后续目录化后可自愈——对齐旧实现「失败只影响当轮」的语义
    #[test]
    fn test_entries_failure_no_poison() {
        let dir = tmp_dir("poison");
        let fake = dir.join("fake-dir");
        std::fs::write(&fake, "x").unwrap(); // 文件冒充目录：read_dir 必失败
        let mut w = GlobWalker::default();
        assert!(w.entries(&fake).is_empty());
        assert!(!w.dir_cache.contains_key(&fake), "失败不得缓存空结果");
        // 自愈：文件删除、真目录建立后，同实例重探即命中。Windows 对「删除后
        // 同名重建」的元数据传播有短延迟，轮询至多 1s 容忍（核心断言是上面
        // 的"不缓存空结果"，此处只验证失败路径不留永久毒缓存）
        std::fs::remove_file(&fake).unwrap();
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(fake.join("a.jsonl"), "x").unwrap();
        let mut healed = 0usize;
        for _ in 0..20 {
            healed = w.entries(&fake).len();
            if healed > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(healed, 1, "目录化后 entries 应自愈枚举到");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
