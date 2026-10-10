//! Claude Pro/Max 订阅本地推算适配器（M3-12，LocalEstimate 口径）。
//! 官方不暴露用量指标（05 调研），本地聚合 usage_records 推算订阅窗口消耗：
//!   5h 块＝ccusage blocks 语义移植（rust/crates/ccusage/src/blocks.rs，
//!   2026-09-25 核实）：块起点＝首请求对齐整点（UTC），持续 5h；任一记录距
//!   块起点或距上一记录超 5h 即断块，新块以该记录整点重锚。当前块＝起点
//!   ≤now<起点＋5h 的块。
//!   档位限额（Maciek plans.py 硬编码社区测算值，2026-09-25 核实）：
//!   Pro 19k／Max5 88k／Max20 220k tokens；auto 档＝P90 自动探测（Maciek
//!   p90_calculator.py：近 8 天已完成块撞限优先→全部块兜底→exclusive 90 分位，
//!   下限 19k）。
//!   token 口径＝input＋output＋cache_creation＋cache_read 四项和（不含
//!   reasoning——Claude 无此分项）；仅统计 agent='claude-code'（采集层写入
//!   字面量；与 ccusage 只读 Claude Code 转录同口径——曾误写 'claude' 致
//!   推算恒 0，2026-09-29 审查修复）。
//!   7d 窗口只聚合展示、无限额（2026-09-25 所有者拍板：官方未公开周限额，
//!   Maciek 主分支亦无周窗口实现，不编造）。

use crate::store::{QuotaRow, Store};
use crate::provider::{ProviderAdapter, QuotaSnapshot};
use std::sync::Arc;

const MILLIS_PER_HOUR: i64 = 3_600_000;
/// P90 探测与当前块覆盖的回看窗口
const LOOKBACK_MS: i64 = 8 * 24 * MILLIS_PER_HOUR;
/// 块时长（Claude 订阅窗口）
const BLOCK_MS: i64 = 5 * MILLIS_PER_HOUR;
/// 7 天窗口
const WEEK_MS: i64 = 7 * 24 * MILLIS_PER_HOUR;
/// P90 兜底下限（= Pro 档限额，Maciek DEFAULT_TOKEN_LIMIT）
const DEFAULT_LIMIT: i64 = 19_000;
/// 撞限判定档位表（Maciek COMMON_TOKEN_LIMITS）
const COMMON_LIMITS: [i64; 4] = [19_000, 88_000, 220_000, 880_000];

pub struct ClaudeLocalProvider {
    /// 实例归属（快照入库 account_id）
    account_id: String,
    /// 本地库（聚合查询；无外呼——宪法红线④的纯增强层）
    store: Arc<Store>,
}

impl ClaudeLocalProvider {
    pub fn new(account_id: &str, store: Arc<Store>) -> Self {
        Self {
            account_id: account_id.to_string(),
            store,
        }
    }
}

impl ProviderAdapter for ClaudeLocalProvider {
    fn id(&self) -> &'static str {
        "claude-local"
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    /// 本地聚合推算：无外呼、无凭据；查询失败按惯例留痕降级为最近快照
    fn fetch(&self) -> anyhow::Result<QuotaSnapshot> {
        let result = (|| -> anyhow::Result<QuotaSnapshot> {
            let now = now_ms();
            let rows = self.store.claude_usage_rows(now - LOOKBACK_MS)?;
            let plan = self.store.get_setting("claude_plan").unwrap_or_default();
            Ok(QuotaSnapshot::Windows(build_quota_rows(
                &rows, now, &plan, &self.account_id,
            )))
        })();
        match result {
            Ok(snap) => {
                log::debug!("Claude 本地推算完成");
                Ok(snap)
            }
            Err(e) => {
                log::warn!("Claude 本地推算失败（降级为最近快照）：{e}");
                Err(e)
            }
        }
    }
}

// ---------- 聚合纯函数（ccusage/Maciek 语义移植，单测锁定） ----------

/// 一个 5h 块：起点（整点锚定）＋块内 token 合计
#[derive(Debug, Clone, Copy, PartialEq)]
struct Block {
    start: i64,
    tokens: i64,
    /// 含 now（当前正在进行的块不参与 P90）
    active: bool,
}

/// ccusage identify_session_blocks 语义移植：块起点＝首请求对齐整点；任一
/// 记录距块起点或距上一记录超 5h 即断块（gap 不生成——零 token 块对 P90
/// 与当前块判定无影响，仅保留重锚语义）
fn split_blocks(rows: &[(i64, i64)], now: i64) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut cur_start: Option<i64> = None;
    let mut cur_tokens = 0i64;
    let mut last_ts: Option<i64> = None;
    for &(ts, tokens) in rows {
        match cur_start {
            Some(start) => {
                let since_start = ts - start;
                let since_last = ts - last_ts.unwrap_or(ts);
                if since_start > BLOCK_MS || since_last > BLOCK_MS {
                    blocks.push(Block {
                        start,
                        tokens: cur_tokens,
                        active: start <= now && now < start + BLOCK_MS,
                    });
                    // 新块以该记录整点重锚（ccusage floor_to_hour 语义）
                    cur_start = Some(ts - ts.rem_euclid(MILLIS_PER_HOUR));
                    cur_tokens = tokens;
                } else {
                    cur_tokens += tokens;
                }
            }
            None => {
                cur_start = Some(ts - ts.rem_euclid(MILLIS_PER_HOUR));
                cur_tokens = tokens;
            }
        }
        last_ts = Some(ts);
    }
    if let Some(start) = cur_start {
        blocks.push(Block {
            start,
            tokens: cur_tokens,
            active: start <= now && now < start + BLOCK_MS,
        });
    }
    blocks
}

/// P90 档位探测（Maciek p90_calculator.py 移植）：近 8 天已完成（非 active）
/// 非零块，优先"撞限"块（≥任一档位 ×0.95），无撞限则全部块兜底；
/// exclusive 90 分位（Python statistics.quantiles n=10 第 9 切点同式），
/// 下限 DEFAULT_LIMIT
fn p90_limit(blocks: &[Block]) -> i64 {
    let done: Vec<i64> = blocks
        .iter()
        .filter(|b| !b.active && b.tokens > 0)
        .map(|b| b.tokens)
        .collect();
    if done.is_empty() {
        return DEFAULT_LIMIT;
    }
    let hits: Vec<i64> = done
        .iter()
        .copied()
        .filter(|t| COMMON_LIMITS.iter().any(|l| *t as f64 >= *l as f64 * 0.95))
        .collect();
    let sample = if hits.is_empty() { &done } else { &hits };
    let mut sorted: Vec<f64> = sample.iter().map(|t| *t as f64).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // exclusive 法第 9/10 切点：位置 = 9*(N+1)/10（1-based），相邻插值
    let n = sorted.len() as f64;
    let pos = 9.0 * (n + 1.0) / 10.0;
    let idx = (pos.floor().max(1.0) as usize).min(sorted.len()) - 1;
    let frac = pos - pos.floor();
    let q = if idx + 1 >= sorted.len() {
        sorted[sorted.len() - 1]
    } else {
        sorted[idx] + frac * (sorted[idx + 1] - sorted[idx])
    };
    (q as i64).max(DEFAULT_LIMIT)
}

/// 档位限额：pro/max5/max20 固定值（Maciek plans.py），auto＝P90 探测
/// （未知值按 auto 处理）
fn plan_limit(plan: &str, blocks: &[Block]) -> i64 {
    match plan {
        "pro" => 19_000,
        "max5" => 88_000,
        "max20" => 220_000,
        _ => p90_limit(blocks),
    }
}

/// 聚合 → 两行窗口快照：5h（当前块 token／档位限额，reset_at＝块起点＋5h；
/// 无活动块＝窗口已重置，显 0%）＋7d（滚动合计，限额未公开不产百分比）
fn build_quota_rows(rows: &[(i64, i64)], now: i64, plan: &str, account_id: &str) -> Vec<QuotaRow> {
    let blocks = split_blocks(rows, now);
    let limit = plan_limit(plan, &blocks) as f64;
    let block = blocks.iter().find(|b| b.active).copied();
    let (used, reset_at) = match block {
        Some(b) => (b.tokens, Some(b.start + BLOCK_MS)),
        None => (0, None), // 无活动块＝窗口已重置，新窗口零用量
    };
    let week_tokens: i64 = rows
        .iter()
        .filter(|(ts, _)| *ts >= now - WEEK_MS)
        .map(|(_, t)| *t)
        .sum();
    let fetched = now;
    vec![
        QuotaRow {
            provider: "claude-local".into(),
            account_id: Some(account_id.into()),
            window_kind: "5h".into(),
            used_percent: Some(used as f64 / limit * 100.0),
            used_tokens: Some(used),
            reset_at,
            fetched_at: fetched,
        },
        QuotaRow {
            provider: "claude-local".into(),
            account_id: Some(account_id.into()),
            window_kind: "weekly".into(),
            // 限额未公开（2026-09-25 所有者拍板）：只聚合展示不产百分比，
            // 岛告急/水位分色对 7d 自然不参与（tensestQuota 过滤 None）
            used_percent: None,
            used_tokens: Some(week_tokens),
            reset_at: None,
            fetched_at: fetched,
        },
    ]
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = MILLIS_PER_HOUR;
    /// 某天 10:00:00 起（UTC 毫秒取整小时方便断言）
    fn hour(n: i64) -> i64 {
        n * H
    }

    #[test]
    fn test_block_anchor_and_split() {
        // 10:03 首请求 → 块锚定 10:00；10:20/14:00 同块（距起点 4h）；19:00
        // 距起点 9h>5h 断块，新块锚 19:00
        let rows = vec![
            (hour(10) + 3 * 60_000, 100),
            (hour(10) + 20 * 60_000, 50),
            (hour(14), 30),
            (hour(19), 400),
        ];
        let now = hour(11); // 11:00：在 10:00~15:00 首块内
        let blocks = split_blocks(&rows, now);
        assert_eq!(blocks.len(), 2, "19:00 记录应断开新块");
        assert_eq!(blocks[0].start, hour(10), "块起点对齐整点");
        assert_eq!(blocks[0].tokens, 180, "同块 token 累加");
        assert!(blocks[0].active, "now=15:00 在 10:00+5h 内");
        assert_eq!(blocks[1].start, hour(19), "新块以记录整点重锚");
        assert!(!blocks[1].active);
    }

    #[test]
    fn test_gap_break_on_last_entry() {
        // 距上一记录超 5h 也断块（即使仍在块起点 5h 外侧语义下同样重锚）
        let rows = vec![(hour(10), 100), (hour(16) + 10 * 60_000, 200)];
        let now = hour(17);
        let blocks = split_blocks(&rows, now);
        assert_eq!(blocks.len(), 2, "距上一记录 6h>5h 应断块");
        assert_eq!(blocks[1].start, hour(16), "重锚到新记录整点");
        assert_eq!(blocks[1].tokens, 200);
    }

    #[test]
    fn test_p90_prefers_limit_hits_and_formula() {
        // 撞限优先：3 个普通块（10k）＋10 个撞限块（84k~93k，均 ≥88k×0.95），
        // P90 应取撞限样本而非全样本
        let mk = |start: i64, tokens: i64, active: bool| Block { start, tokens, active };
        let mut blocks = vec![
            mk(hour(0), 10_000, false),
            mk(hour(6), 10_000, false),
            mk(hour(12), 10_000, false),
        ];
        for i in 0..10 {
            blocks.push(mk(hour(18 + i), 84_000 + i * 1_000, false));
        }
        // 撞限样本升序 [84k..93k]：exclusive P90 位置 = 9*(10+1)/10 = 9.9 →
        // sorted[8] + 0.9*(sorted[9]-sorted[8]) = 92k + 0.9*1k = 92_900
        assert_eq!(p90_limit(&blocks), 92_900);
    }

    #[test]
    fn test_p90_fallback_all_blocks_and_floor() {
        // 无撞限块→全部块兜底；不足下限→19k 兜底
        let mk = |start: i64, tokens: i64| Block {
            start,
            tokens,
            active: false,
        };
        let blocks = vec![mk(hour(0), 5_000), mk(hour(6), 8_000)];
        assert_eq!(p90_limit(&blocks), DEFAULT_LIMIT, "低用量兜底 19k");
        assert_eq!(p90_limit(&[]), DEFAULT_LIMIT, "无数据兜底 19k");
    }

    #[test]
    fn test_plan_limits_fixed_values() {
        let blocks = vec![];
        assert_eq!(plan_limit("pro", &blocks), 19_000);
        assert_eq!(plan_limit("max5", &blocks), 88_000);
        assert_eq!(plan_limit("max20", &blocks), 220_000);
        assert_eq!(plan_limit("auto", &blocks), DEFAULT_LIMIT);
        assert_eq!(plan_limit("垃圾值", &blocks), DEFAULT_LIMIT, "未知值按 auto");
    }

    #[test]
    fn test_quota_rows_shape() {
        // 当前块 9_500/19_000＝50%；7d 合计剔除窗外记录（rows 按 ts 升序＝SQL 同款）
        let now = hour(12);
        let rows = vec![(now - WEEK_MS - H, 7_000), (hour(10) + 5 * 60_000, 9_500)];
        let qs = build_quota_rows(&rows, now, "pro", "acc-1");
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].window_kind, "5h");
        assert!((qs[0].used_percent.unwrap() - 50.0).abs() < 1e-9);
        assert_eq!(qs[0].used_tokens, Some(9_500));
        assert_eq!(qs[0].reset_at, Some(hour(10) + 5 * H), "reset＝块起点＋5h");
        assert_eq!(qs[0].account_id.as_deref(), Some("acc-1"), "归属传入实例");
        assert_eq!(qs[1].window_kind, "weekly");
        assert_eq!(qs[1].used_tokens, Some(9_500), "7d 合计剔除窗外记录");
        assert_eq!(qs[1].used_percent, None, "周限额未公开不产百分比");
    }

    #[test]
    fn test_no_active_block_means_reset() {
        // 8h 回看内无记录：5h 行显 0%（窗口已重置）、reset 不可知
        let qs = build_quota_rows(&[], hour(12), "pro", "acc-1");
        assert_eq!(qs[0].used_percent, Some(0.0));
        assert_eq!(qs[0].reset_at, None);
    }
}
