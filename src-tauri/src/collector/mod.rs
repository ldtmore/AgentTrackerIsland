//! Agent 采集器模块：定义 AgentAdapter 抽象与公共数据结构。
//! 每个被监控的 Agent（Claude Code、ZCode……）实现一份，禁止 if-else 堆砌（宪法§三）。

use crate::store::UsageRow;

/// 会话元数据（scan 产物，供岛 UI 与状态聚合使用）
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// 自库主键："{agent}：{原始sessionId}"
    pub id: String,
    pub agent: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub project_dir: Option<String>,
    pub title: Option<String>,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    /// 最近一次模型调用时间（毫秒），状态聚合的启发式输入
    pub last_usage_at: Option<i64>,
}

/// 一轮增量采集的产出（2026-09-21 cost-state 校准扩展）：
/// rows 是常规用量流水；cost_snapshots 是 CC 转录 cost-state 行携带的
/// 会话级累计快照（按会话×归一化模型取最大值），供 service 层重算后台差值；
/// titles 是 CC 转录 ai-title 行携带的会话标题（后写覆盖=最新），供会话元数据兜底
#[derive(Debug, Default, Clone)]
pub struct CollectOutput {
    pub rows: Vec<UsageRow>,
    pub cost_snapshots: Vec<CostSnapshot>,
    /// （命名空间会话 id， 最新标题）；ZCode 恒空（其 session 表自带标题）
    pub titles: Vec<(String, String)>,
}

/// 一条 cost-state 累计快照：某会话某模型的官方总账累计（含 assistant 与后台调用）
#[derive(Debug, Clone)]
pub struct CostSnapshot {
    /// 命名空间会话 id（"{agent}:{原始id}"，与 UsageRow.session_id 同口径）
    pub session_id: String,
    /// 归一化模型名（去 [1m] 等上下文后缀，与 assistant 行模型名对齐）
    pub model: String,
    /// 四项 token 累计（该会话该模型的会话级总量）
    pub cumulative_tokens: i64,
    /// 该 cost-state 行的时间戳（毫秒）
    pub ts: i64,
}

/// Agent 适配器抽象：实现者只读不改目标 Agent 的任何数据（红线①）
///
/// M0 采集模型为"定时轮询水位增量"；M2 起为「引擎+声明」模型：
/// 机制（枚举/增量读/节流/快轮探测）在 engine.rs，适配器只声明参数与解析。
pub trait AgentAdapter: Send + Sync {
    /// Agent 标识：'zcode' | 'claude-code' | ...
    fn id(&self) -> &'static str;

    /// 发现会话（元数据，不含用量）
    fn scan_sessions(&self) -> anyhow::Result<Vec<SessionInfo>>;

    /// 增量采集用量：返回 started_at 严格大于 watermark 的调用记录
    /// （cost_snapshots 仅 CC 有，ZCode 源库无 cost-state 对应物，返回空）
    fn collect_usage(&self, watermark_ts: i64) -> anyhow::Result<CollectOutput>;

    /// 快轮信号声明（M2-1）：调度器以 1~2s 高频探测这些目标的采样值，
    /// 值变化才唤醒全量 tick——「回合起点」的亚 10 秒感知来源。
    /// 默认空 = 该 Agent 不参与快轮（仍随常规 tick 被扫描）
    fn hot_signals(&self) -> Vec<engine::HotSignal> {
        vec![]
    }

    /// 进程匹配规则声明（M2-4）：进程枚举按此判定该 Agent 是否存活；
    /// None = 不参与进程探测（对应状态机的 process_alive=false 兜底）
    fn process_match(&self) -> Option<engine::ProcessMatch> {
        None
    }
}

pub mod claude_code;
pub mod engine;
pub mod hook_events;
pub mod zcode;

/// 由模型名推断供应商（启发式，小而够用）
/// ZCode 的 provider_id 是内部 UUID，直接映射不可读；后续可换配置表
pub fn provider_from_model(model: &str) -> Option<String> {
    let p = model.to_ascii_lowercase();
    if p.starts_with("glm") {
        Some("glm".into())
    } else if p.starts_with("claude") {
        Some("anthropic".into())
    } else if p.starts_with("gpt") || p.starts_with("o1") || p.starts_with("o3") || p.starts_with("o4") {
        Some("openai".into())
    } else if p.starts_with("gemini") {
        Some("google".into())
    } else if p.starts_with("deepseek") {
        Some("deepseek".into())
    } else if p.starts_with("kimi") || p.starts_with("moonshot") {
        Some("moonshot".into())
    } else if p.starts_with("qwen") {
        Some("alibaba".into())
    } else {
        None
    }
}
