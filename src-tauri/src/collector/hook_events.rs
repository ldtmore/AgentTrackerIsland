//! hooks 事件文件消费者：增量读取 hook-bridge 写入的事件文件。
//! 协议（hook-bridge.js 白名单字段，实测于 2026-09-16）：
//!   每行 {"ts":毫秒,"hook":"Stop","session_id":"...","tool_name":?,"message":?}
//! 读取按字节偏移增量（文件 append-only），断电/重启后从上次偏移继续——红线③。
//!
//! 2026-09-17 审查优化 1.2:
//!   ① 读法从"整文件 read_to_string"改为 File+seek 到偏移再读尾——读取成本
//!     从 O（全文件） 降到 O（新增字节）（事件文件含每次工具调用一行，会持续增长）；
//!   ② 修复 bug：文件被重建/轮转导致 total < offset 时，旧实现永久停在旧偏移、
//!     事件消费从此失明；现检测到文件变小即归零重读；
//!   ③ 新增 rotate_if_large：已全部消费且超阈值时滚动为 .old，防无限膨胀。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 事件文件轮转阈值（8MB）：PreToolUse/PostToolUse 每次工具调用一行，
/// 活跃使用数月可轻松超过；滚动保留一代 .old 即可
pub const MAX_EVENT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// 一条 hook 状态事件（Serialize 供审计落库 status_events）
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct HookEvent {
    pub ts: i64,
    pub hook: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// 事件根目录：%LOCALAPPDATA%\AgentTrackerIsland\events
/// （每家 Agent 一个事件文件，互不干扰——04-EXPANSION §2.3.3）
pub fn events_dir() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("AgentTrackerIsland").join("events"))
}

/// 事件文件路径（按 Agent 隔离）：%LOCALAPPDATA%\AgentTrackerIsland\events\<agent>.jsonl。
/// claude-code 的文件名与多 Agent 化之前一致，历史事件文件自然沿用（零迁移）
pub fn events_file_path(agent: &str) -> Option<PathBuf> {
    Some(events_dir()?.join(format!("{agent}.jsonl")))
}

/// 增量读取：返回（新事件， 新偏移）。
/// 偏移语义：已消费的字节位置，只会推进到最后一个完整换行处——
/// 上次停在半行中间则先跳到下一个换行后再解析；文件不存在/被占用本轮静默跳过（红线④）。
pub fn read_events(path: &Path, offset: u64) -> anyhow::Result<(Vec<HookEvent>, u64)> {
    let mut events = vec![];
    let Ok(mut f) = std::fs::File::open(path) else {
        return Ok((events, offset)); // 尚未安装/文件暂被占用：静默
    };
    let total = match f.metadata() {
        Ok(m) => m.len(),
        Err(e) => return Err(anyhow::anyhow!("读取事件文件元数据失败：{e}")),
    };
    // 文件比偏移还小 = 被重建/轮转：偏移归零，当轮立即重读（bug 修复，审查 1.2；
    // 该状态迁移是"事件消费失明"bug 的现场，debug 留痕——埋点审查 2026-09-17）
    let offset = if total < offset {
        log::debug!("[hooks] 事件文件被重建/轮转（长度 {total} < 偏移 {offset}），归零重读");
        0
    } else {
        offset
    };
    if total == offset {
        return Ok((events, offset)); // 无新增
    }
    // 定位解析起点：检查 offset 前一字节是否换行；停在半行则跳到下一个换行之后
    let mut start = offset;
    if offset > 0 {
        f.seek(SeekFrom::Start(offset - 1))?;
        let mut prev = [0u8; 1];
        if f.read_exact(&mut prev).is_err() {
            log::debug!("[hooks] 偏移前一字节读取失败（本轮跳过）");
            return Ok((events, offset));
        }
        if prev[0] != b'\n' {
            let mut rest = Vec::new();
            if f.read_to_end(&mut rest).is_err() {
                return Ok((events, offset));
            }
            match rest.iter().position(|b| *b == b'\n') {
                Some(i) => start = offset + i as u64 + 1,
                None => return Ok((events, offset)), // 半行尚未写完：本轮不推进
            }
        }
    }
    if f.seek(SeekFrom::Start(start)).is_err() {
        log::debug!("[hooks] 事件文件 seek 失败（本轮跳过）");
        return Ok((events, offset));
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        log::debug!("[hooks] 事件文件读取失败（文件被占用？本轮跳过）");
        return Ok((events, offset)); // 读失败（占用等）：本轮跳过
    }
    // 逐完整行解析（按字节切行，偏移推进与文件字节严格对应；
    // 单行内 UTF-8 损坏只影响该行，不影响偏移）
    let mut new_offset = start;
    let mut consumed = 0usize;
    let mut bad_lines = 0usize;
    for line in buf.split_inclusive(|b| *b == b'\n') {
        if !line.ends_with(b"\n") {
            break; // 末尾半行：留给下一轮
        }
        consumed += line.len();
        new_offset = start + consumed as u64;
        let text = String::from_utf8_lossy(line);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<HookEvent>(trimmed) {
            events.push(ev);
        } else {
            bad_lines += 1;
        } // 坏行跳过不阻塞，计数留痕
    }
    // 坏行留痕：正常为零；持续出现 = hook-bridge 协议变更或文件损坏
    // （若无此留痕，事件会"静默全丢"——埋点审查 2026-09-17）
    if bad_lines > 0 {
        log::debug!("[hooks] 本轮 {bad_lines} 行解析失败已跳过");
    }
    Ok((events, new_offset))
}

/// 事件文件轮转：已全部消费（offset == 文件长）且超过 max_bytes 阈值时，
/// rename 为 `*.jsonl.old`（覆盖上一代），返回新偏移 0；否则原样返回 offset。
/// 调用时机：每轮事件消费完成之后（未消费完不轮转，杜绝丢事件）。
/// max_bytes 由调用方传 MAX_EVENT_FILE_BYTES（参数化以便单测）
pub fn rotate_if_large(path: &Path, offset: u64, max_bytes: u64) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return offset;
    };
    if meta.len() <= max_bytes || meta.len() > offset {
        return offset; // 未超限，或尚有未消费字节
    }
    let old = path.with_extension("jsonl.old");
    if old.exists() {
        let _ = std::fs::remove_file(&old);
    }
    match std::fs::rename(path, &old) {
        Ok(()) => {
            log::info!(
                "hook 事件文件已达 {} 字节，已轮转为 {}，偏移归零",
                meta.len(),
                old.display()
            );
            0
        }
        Err(e) => {
            log::warn!("hook 事件文件轮转失败（下轮再试）：{e}");
            offset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("at-t6-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_read_events_incremental() {
        let dir = tmp_dir("incr");
        let f = dir.join("events.jsonl");

        // 第一次：两行
        std::fs::write(&f, concat!(
            r#"{"ts":1,"hook":"SessionStart","session_id":"s1"}"#, "\n",
            r#"{"ts":2,"hook":"UserPromptSubmit","session_id":"s1"}"#, "\n"
        )).unwrap();
        let (evs, off) = read_events(&f, 0).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].hook, "SessionStart");
        assert!(off > 0);

        // 无新增：空
        let (evs2, off2) = read_events(&f, off).unwrap();
        assert!(evs2.is_empty());
        assert_eq!(off2, off);

        // 追加一行（模拟半行竞态后再写全）
        std::fs::write(&f, format!(
            "{}{}",
            std::fs::read_to_string(&f).unwrap(),
            concat!(r#"{"ts":3,"hook":"Stop","session_id":"s1","message":"done"}"#, "\n")
        )).unwrap();
        let (evs3, _) = read_events(&f, off2).unwrap();
        assert_eq!(evs3.len(), 1);
        assert_eq!(evs3[0].hook, "Stop");
        assert_eq!(evs3[0].message.as_deref(), Some("done"));

        // 文件不存在：空且偏移不变
        let (evs4, off4) = read_events(&dir.join("nope.jsonl"), 42).unwrap();
        assert!(evs4.is_empty());
        assert_eq!(off4, 42);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bug 修复回归：文件被重建（total < offset）后偏移必须归零重读，
    /// 否则事件消费从此永久失明（审查 1.2）
    #[test]
    fn test_recreated_file_resets_offset() {
        let dir = tmp_dir("reset");
        let f = dir.join("events.jsonl");
        // 999 个 x + 换行 = 1000 字节（偏移只推进到最后一个完整换行）
        std::fs::write(&f, format!("{}\n", "x".repeat(999))).unwrap();
        let (_, big_off) = read_events(&f, 0).unwrap();
        assert_eq!(big_off, 1000);

        // 模拟轮转/用户清理：文件重建为全新小文件
        std::fs::remove_file(&f).unwrap();
        std::fs::write(&f, concat!(
            r#"{"ts":9,"hook":"Stop","session_id":"s2"}"#, "\n"
        )).unwrap();
        let (evs, new_off) = read_events(&f, big_off).unwrap();
        assert_eq!(evs.len(), 1, "重建后应归零重读到新事件");
        assert_eq!(evs[0].session_id, "s2");
        assert!(new_off > 0 && new_off < big_off);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 轮转：已消费完 + 超阈值 → 滚动 .old 且偏移归零；未消费完 → 不轮转
    #[test]
    fn test_rotate_if_large() {
        let dir = tmp_dir("rotate");
        let f = dir.join("events.jsonl");
        std::fs::write(&f, "x".repeat(2000)).unwrap();

        // 未消费完（len > offset）：不轮转
        assert_eq!(rotate_if_large(&f, 10, 1000), 10);
        assert!(f.exists());

        // 已消费完但未超阈值：不轮转
        assert_eq!(rotate_if_large(&f, 2000, 8000), 2000);
        assert!(f.exists());

        // 已消费完 + 超限：滚动为 .old，返回偏移 0
        assert_eq!(rotate_if_large(&f, 2000, 1000), 0);
        assert!(!f.exists());
        assert!(f.with_extension("jsonl.old").exists());

        // 二次轮转：旧 .old 被覆盖
        std::fs::write(&f, "y".repeat(1500)).unwrap();
        assert_eq!(rotate_if_large(&f, 1500, 1000), 0);
        let content = std::fs::read_to_string(f.with_extension("jsonl.old")).unwrap();
        assert!(content.starts_with('y'), "旧 .old 应被新一代覆盖");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 半行恢复：offset 落在半行中间，先跳过残行再解析完整行
    #[test]
    fn test_partial_line_resume() {
        let dir = tmp_dir("partial");
        let f = dir.join("events.jsonl");
        let full = concat!(r#"{"ts":1,"hook":"Stop","session_id":"s1"}"#, "\n");
        let half = r#"{"ts":2,"hook":"Notification""#; // 无换行的半行
        std::fs::write(&f, format!("{full}{half}")).unwrap();
        let (evs, off) = read_events(&f, 0).unwrap();
        assert_eq!(evs.len(), 1); // 半行未写完：不产生事件
        // 半行补全
        std::fs::write(&f, format!("{full}{}{}\n", half, r#","session_id":"s2"}"#)).unwrap();
        let (evs2, _) = read_events(&f, off).unwrap();
        assert_eq!(evs2.len(), 1);
        assert_eq!(evs2[0].session_id, "s2");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
