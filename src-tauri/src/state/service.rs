//! 聚合服务：定时 tick，把采集器/事件/额度融合为岛快照（IslandSnapshot）。
//! 调度策略：每 tick（建议 10s）做采集+状态计算；GLM 额度每 5 分钟刷新一次，
//! 刷新失败降级显示最近快照（红线④）。T8/T9 由 Tauri 后台线程驱动并向前端广播。
//!
//! 2026-09-17 审查优化：
//!   ① 适配器注册表（审查 3.6）：Vec<Box<dyn AgentAdapter>>，新 Agent 即插即用；
//!   ② degraded 标志（审查 1.1）：连续多轮采集源失败 → 快照带降级位，前端可见，
//!      配合文件日志终结"静默失明"；
//!   ③ 进程枚举复用 System 实例且降频到 30s（审查 2.2.4）；
//!   ④ 会话 token/模型改批量查询（审查 2.2.2，消除逐会话 N+1）；
//!   ⑤ hook 事件文件消费失败留痕 + 已消费完超限自动轮转（审查 1.2）。

use std::collections::HashMap;
use std::sync::Arc;

use crate::collector::aider::AiderAdapter;
use crate::collector::claude_code::ClaudeCodeAdapter;
use crate::collector::codex::CodexAdapter;
use crate::collector::copilot::CopilotAdapter;
use crate::collector::engine::{HotSignal, ProcessMatch};
use crate::collector::gemini::GeminiAdapter;
use crate::collector::goose::GooseAdapter;
use crate::collector::hermes::HermesAdapter;
use crate::collector::hook_events;
use crate::collector::kimi::KimiCodeAdapter;
use crate::collector::openclaw::OpenClawAdapter;
use crate::collector::opencode::OpenCodeFamilyAdapter;
use crate::collector::qwen::QwenCodeAdapter;
use crate::collector::zcode::ZcodeAdapter;
use crate::collector::{AgentAdapter, provider_from_model};
use crate::store::UsageRow;
use crate::state::{
    aggregate, compute_state, is_failure_event, ERROR_FRESH_MS, IslandState, SessionSignals,
    SessionState,
};
use crate::store::Store;

/// 额度刷新间隔／退避台阶／启动错峰常量：已随调度拆分迁至 provider/worker.rs
/// （2026-09-29 审查修复，见该文件头注释）
///
/// 水位安全余量（毫秒，R4）：并发会话慢刷盘的行 ts 可能略小于其他会话推进的
/// 全局水位，按原始水位过滤会永久丢行；回退 60s 重采，幂等键保证不重复入库
const WATERMARK_MARGIN_MS: i64 = 60 * 1000;
/// 进程枚举间隔（毫秒，审查 2.2.4）：全量刷新开销可观，且进程存亡只影响
/// idle/offline 粒度，30s 精度足够
const PROBE_INTERVAL_MS: i64 = 30 * 1000;
/// 连续失败多少轮判定 degraded（10s/轮，3 轮 ≈ 30s：瞬时抖动不闪红）
const DEGRADE_AFTER_TICKS: u32 = 3;
/// 单适配器单轮采集耗时阈值（毫秒，#17 看门狗）：超时计一次"慢"——
/// 红线"故障隔离"对 Err/panic 成立、对"慢/挂起"原不成立（坏盘/超大库
/// 拖慢整轮串行 tick，快轮唤醒的实时性退化为无界等待）
const ADAPTER_SLOW_MS: u128 = 5_000;
/// 连续慢多少轮进入冷却（与 degraded 同款 3 轮防瞬时抖动）
const ADAPTER_SLOW_STREAKS: u32 = 3;
/// 冷却时长（毫秒）：期间跳过该适配器采集，其余 Agent 恢复正常节拍；
/// 冷却结束自动恢复——首轮全量补采，幂等键保证零副作用（红线③）
const ADAPTER_COOLDOWN_MS: i64 = 60_000;

/// 展示用会话视图（serde 给前端）。
/// 2026-09-18 展示改造：新增四项 token 拆解（账单口径）与最近错误类型
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionView {
    pub id: String,
    pub agent: String,
    pub model: Option<String>,
    pub project_dir: Option<String>,
    pub title: Option<String>,
    pub state: SessionState,
    /// 总消耗 = 四项相加（与官方账单同口径，含缓存）
    pub session_tokens: i64,
    /// 四项拆解（面板 tooltip 展示输入/输出/缓存读/缓存写）
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    /// 该会话最近一次调用出错的类型（不限时间窗；前端仅在 state=error 时展示原因）
    pub error_type: Option<String>,
    pub last_activity_at: Option<i64>,
}

/// 展示用额度视图（account_id 为实例归属，M3-2 起新增；前端可选消费）。
/// fetched_at 为快照抓取时间（M3-5 透传：此前前端只能拿 reset_at（未来的
/// 重置时点）冒充数据时间，额度页脚注/设置页摘要显示成未来时刻）
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuotaView {
    pub provider: String,
    pub account_id: Option<String>,
    pub window_kind: String,
    pub used_percent: Option<f64>,
    pub reset_at: Option<i64>,
    pub fetched_at: i64,
}

impl From<&crate::store::QuotaRow> for QuotaView {
    fn from(q: &crate::store::QuotaRow) -> Self {
        QuotaView {
            provider: q.provider.clone(),
            account_id: q.account_id.clone(),
            window_kind: q.window_kind.clone(),
            used_percent: q.used_percent,
            reset_at: q.reset_at,
            fetched_at: q.fetched_at,
        }
    }
}

/// 实例余额的岛端投影（M3-6）：BalanceRow 去 account_id（归属由外层实例视图承载）
#[derive(Debug, Clone, serde::Serialize)]
pub struct IslandBalanceView {
    pub currency: String,
    pub total: f64,
    pub granted: Option<f64>,
    pub fetched_at: i64,
}

/// 岛/托盘共用的供应商实例视图（M3-6，06-PLAN §9）：多实例轮播与紧张度归一
/// 的数据源。厂商名/品牌色/口径是注册表投影（kind_of）——前端零硬编码，
/// 随 island-snapshot 广播下发，免新增 invoke（岛窗口早期 invoke 竞态面不扩大）
#[derive(Debug, Clone, serde::Serialize)]
pub struct IslandAccountView {
    pub id: String,
    pub kind_id: String,
    /// 厂商显示名（如"GLM"/"Z.ai"；注册表双站变体投影，缺该厂商时兜底 kind_id）
    pub kind_name: String,
    /// 品牌色（指示器圆点/托盘明细色点）
    pub color: String,
    /// "windows" | "balance" | "local_estimate"（前端按口径分型渲染）
    pub quota_kind: String,
    /// 实例别名（用户可改；岛/托盘展示用）
    pub alias: String,
    /// 岛展示集标记（岛端过滤；托盘用全量启用实例）
    pub in_island: bool,
    /// 该实例最新窗口快照（Windows 口径）
    pub quotas: Vec<QuotaView>,
    /// 该实例最新余额快照（Balance 口径）
    pub balance: Option<IslandBalanceView>,
}

/// 实例视图组装（M3-6）：仅启用实例（停用实例的快照是陈旧历史，额度页
/// 「已停用」徽标承载，岛/托盘不展示）；快照按实例归属分组；unknown kind
/// 兜底 kind_id＋中性色，不因注册表缺口阻塞快照
pub(crate) fn island_accounts(store: &Store, latest: &[crate::store::QuotaRow]) -> Vec<IslandAccountView> {
    store
        .list_provider_accounts()
        .into_iter()
        .filter(|a| a.enabled)
        .map(|a| {
            let kind = crate::provider::kind_of(&a.kind_id);
            IslandAccountView {
                quotas: latest
                    .iter()
                    .filter(|q| q.account_id.as_deref() == Some(a.id.as_str()))
                    .map(QuotaView::from)
                    .collect(),
                balance: store.latest_balance(&a.id).map(|b| IslandBalanceView {
                    currency: b.currency,
                    total: b.total,
                    granted: b.granted,
                    fetched_at: b.fetched_at,
                }),
                kind_name: crate::provider::runtime_kind_name(
                    &a.kind_id,
                    a.base_override.as_deref(),
                ),
                color: kind
                    .map(|k| k.color.to_string())
                    .unwrap_or_else(|| "#8a8f98".into()),
                quota_kind: kind
                    .map(|k| match k.quota_kind {
                        crate::provider::QuotaKind::Windows => "windows",
                        crate::provider::QuotaKind::Balance => "balance",
                        crate::provider::QuotaKind::LocalEstimate => "local_estimate",
                    })
                    .unwrap_or_default()
                    .to_string(),
                id: a.id,
                kind_id: a.kind_id,
                alias: a.alias,
                in_island: a.in_island,
            }
        })
        .collect()
}

/// 岛快照：一次 tick 的完整产出。
/// 2026-09-18 展示改造：新增今日口径（today_*）；
/// M3-6：额度段改按实例视图（accounts）展示，glm_configured 标志随之移除
/// （岛/托盘/面板全部改走实例视图，单真值源）
#[derive(Debug, Clone, serde::Serialize)]
pub struct IslandSnapshot {
    pub sessions: Vec<SessionView>,
    pub island: IslandState,
    pub quotas: Vec<QuotaView>,
    /// 供应商实例视图（M3-6）：启用实例全量（in_island 标记由岛端过滤）
    pub accounts: Vec<IslandAccountView>,
    /// GLM 5h 额度已耗尽（100%）：不改写会话状态，由前端驱动胶囊变红/标签红光
    pub quota_exhausted: bool,
    /// 采集源连续失败（审查 1.1）：前端在收缩态提示"采集异常"，
    /// 与"没有会话"区分开——降级必须可见，不允许静默失明
    pub degraded: bool,
    /// 今日 token 总量（本机今日零点起，账单口径含缓存）
    pub today_tokens: i64,
    /// 今日模型调用次数（本机今日零点起）
    pub today_calls: i64,
    pub generated_at: i64,
}

/// 聚合器（有状态：水位/事件偏移/进程探测缓存）。供应商额度外呼已拆至
/// provider/worker.rs 独立线程（2026-09-29 审查修复：fetch 阻塞不再绑架
/// 聚合 tick 与快照广播），本结构只负责采集＋状态融合＋快照组装
pub struct Aggregator {
    store: Arc<Store>,
    /// Agent 适配器注册表（审查 3.6）：新增 Agent = 实现 trait 后在此登记，
    /// 采集主循环不出现 per-agent if-else
    adapters: Vec<Box<dyn AgentAdapter>>,
    /// hook 事件文件的消费偏移（M2-6/7 多 Agent 泛化）：agent id → 偏移；
    /// 持久化于 app_settings 键 `hook_events_offset:<agent>`
    hook_offsets: HashMap<String, u64>,
    /// 进程枚举复用实例（审查 2.2.4）：避免每 tick 重建 sysinfo 全量快照
    sys: sysinfo::System,
    /// （zcode 活着， claude 活着）探测缓存 → M2-4 起为 {agent_id: 是否存活} 表，
    /// 由各适配器的 process_match 声明驱动
    probe_cache: HashMap<String, bool>,
    last_probe_ms: i64,
    /// 连续采集失败轮数（degraded 判定输入）
    fail_streak: u32,
    /// 最近一次采集错误描述（degraded 根因留痕用；成功轮清空）
    last_error: Option<String>,
    /// 上轮会话状态缓存（首见/迁移/消失留痕用；内存态，重启后首轮视为全量首见）
    last_states: HashMap<String, SessionState>,
    /// sessions 行写入去重缓存（2026-10-03 审查优化）：会话 id → 上次已入库的
    /// 行签名（可变字段的生效值）。快档 1s tick 下逐会话无条件 upsert 是纯写
    /// 放大——空闲会话的 last_seen/state/标题全部稳定，签名未变即跳过写库；
    /// 内存态，重启后首轮全量重写一遍（幂等无害）。仅在写库成功后更新缓存
    session_rows: HashMap<String, SessionRowSig>,
    /// 上轮额度耗尽标志（翻转留痕用）
    last_quota_exhausted: bool,
    /// cost-state 校准缓存：会话×模型 → 上次入库的累计值（值未变则跳过重算；
    /// 内存态，重启后首轮全量重算一遍，幂等无害）
    last_cost_cum: HashMap<(String, String), i64>,
    /// 适配器看门狗（#17）：agent_id →（连续慢轮数， 冷却截止时刻）——单轮超
    /// 5s 计慢，连续 3 轮冷却 60s 跳过（慢故障隔离，见常量注释）
    adapter_slow: HashMap<String, (u32, i64)>,
}

/// sessions 行签名（写入去重用）：与 upsert SQL 的生效语义逐字段对齐——
/// COALESCE 字段记「新值为 Some 时的生效值」、last_seen_at 单调前进、
/// state 无条件覆盖。任一字段变化才需要真正写库
#[derive(PartialEq)]
struct SessionRowSig {
    provider: Option<String>,
    model: Option<String>,
    project_dir: Option<String>,
    title: Option<String>,
    /// 生效 last_seen_at（本输入与库值的 MAX 结果＝两者较大者）
    last_seen_at: i64,
    state: String,
}

impl Aggregator {
    pub fn new(store: Arc<Store>) -> Self {
        // hook 事件消费偏移键（M2-3 per-agent 化）：新键 hook_events_offset:<agent>；
        // 旧单键 hook_events_offset 首启迁移划归 claude-code（历史唯一有 hooks 的 Agent）。
        // M2-6/7 起按适配器逐家初始化（Codex/Kimi 无历史键即从 0 起）
        const HOOK_OFFSET_PREFIX: &str = "hook_events_offset";
        let claude_offset = {
            let key = format!("{HOOK_OFFSET_PREFIX}:claude-code");
            match store.get_setting(&key) {
                Some(v) => v.parse().unwrap_or(0),
                None => match store.get_setting(HOOK_OFFSET_PREFIX) {
                    Some(old) => {
                        store.set_setting(&key, &old);
                        log::info!("[hooks] 消费偏移键迁移：{HOOK_OFFSET_PREFIX} → {key}");
                        old.parse().unwrap_or(0)
                    }
                    None => 0,
                },
            }
        };
        let hook_offsets: HashMap<String, u64> = HashMap::from([("claude-code".into(), claude_offset)]);
        // 0003 重建流程（2026-09-21 数据准确性治理）：迁移已清空 usage_records，
        // 此处归零水位并摘除标志——首轮 collect_usage(0) 全量回溯自动重建
        // （数据源 CC 转录/ZCode 源库都是完整事实源，重建零损失）
        if store.get_setting(crate::store::REBUILD_PENDING_KEY).as_deref() == Some("1") {
            store.clear_watermarks();
            store.set_setting(crate::store::REBUILD_PENDING_KEY, "0");
            log::info!("[重建] 幂等键升级（0003）：水位已归零，首轮采集将全量回溯重建用量数据");
        }
        let mut adapters: Vec<Box<dyn AgentAdapter>> = vec![
                Box::new(ZcodeAdapter::new()),
                // Claude Code 同族两实例（M2-17 家族化）：同一 struct 不同
                // FamilyProfile，差异仅 agent id/转录根/重定向变量/hooks 可用性
                // （01-RESEARCH §19：CodeBuddy Code 与 CC 转录格式全同构）
                Box::new(ClaudeCodeAdapter::claude_code()),
                Box::new(ClaudeCodeAdapter::codebuddy()),
                // Qoder 双版实例（M2-18）：CN（原通义灵码）与国际版账号不互通但
                // 本地格式同构，共享 agent id "qoder"（Codex/Kimi 多根同 id 先例，
                // 会话 id 为 UUID 跨根唯一）——设置页呈现为单一条目
                Box::new(ClaudeCodeAdapter::qoder_cn()),
                Box::new(ClaudeCodeAdapter::qoder_intl()),
                // OpenCode 同族两实例（M2-10a）：同一 struct 不同 FamilyProfile，
                // 差异仅 agent id/数据根/db 文件名（01-RESEARCH §12）
                Box::new(OpenCodeFamilyAdapter::opencode()),
                Box::new(OpenCodeFamilyAdapter::mimo_code()),
                // OpenClaw（M2-13）：多 agent 多库（每 agentId 一库，目录枚举）
                Box::new(OpenClawAdapter::new()),
                // Hermes（M2-14）：state.db 累计快照重采（库内无逐调用流水表，
                // 01-RESEARCH §15）；无 hooks 注入（shell hooks 为 YAML＋consent
                // allowlist，成本高且 SQLite 通道已覆盖，列装机后增强档）
                Box::new(HermesAdapter::new()),
                // Copilot CLI（M2-15）：session-store.db 逐调用流水（rowid 水位，
                // 01-RESEARCH §16）；无 hooks 注入（lifecycle hooks 形态未核实，
                // SQLite 通道已覆盖，列装机后增强档）
                Box::new(CopilotAdapter::new()),
                // Goose（M2-16）：sessions.db 逐调用流水 usage_ledger（毫秒时间戳
                // 水位，01-RESEARCH §18）；无 hooks 注入（goose hooks/recipes 为
                // 自有扩展体系不适用），旧版 JSONL 会话不采（仅读现行 SQLite）
                Box::new(GooseAdapter::new()),
                // Aider（M2-19）：降级档——markdown 历史只提供会话活跃与标题，
                // collect_usage 恒空（无本地 token 记账，01-RESEARCH §21）；
                // 额度/消耗显示按无数据降级，符合红线④
                Box::new(AiderAdapter::new()),
                // WorkBuddy 双根实例（M2-20）：办公智能体，JSONL 转录与 CodeBuddy
                // 同构（腾讯系 message+role 形态，家族解析双形态兼容）；旧根
                // ~/.workbuddy 与 5.5+ 新根 ~/.workbuddy-ai 同 id 多根（幂等去重）；
                // Managed Agents 云端托管不在本机，明确不可观测
                Box::new(ClaudeCodeAdapter::workbuddy()),
                Box::new(ClaudeCodeAdapter::workbuddy_ai()),
        ];
        // Codex＋Kimi Code 多根并存注册（2026-09-29 审查修复 #18）：默认根＋env
        // 重定向根各一个实例（同 id 共享水位/幂等键命名空间，会话 id 跨根唯一
        // 不冲突；hooks 事件文件同 id 单文件、第二个实例增量读到空集天然去重；
        // 会话消失检测已改为全体实例并集判定）
        for root in crate::collector::codex::data_roots() {
            adapters.push(Box::new(CodexAdapter::with_root(root)));
        }
        for root in crate::collector::kimi::data_roots() {
            adapters.push(Box::new(KimiCodeAdapter::with_root(root)));
        }
        // Gemini CLI＋Qwen Code（M2-11）：fork 已分叉故各自独立适配器（调研推翻
        // 「同族参数化换根即用」预想，01-RESEARCH §13）；#18 多根并存＝默认根＋
        // env 重定向根各一实例（同 codex/kimi）
        for root in crate::collector::gemini::data_roots() {
            adapters.push(Box::new(GeminiAdapter::with_root(root)));
        }
        for (root, settings) in crate::collector::qwen::runtime_roots() {
            adapters.push(Box::new(QwenCodeAdapter::with_roots(root, settings)));
        }
        Self {
            store,
            adapters,
            hook_offsets,
            sys: sysinfo::System::new(),
            probe_cache: HashMap::new(),
            last_probe_ms: 0,
            fail_streak: 0,
            last_error: None,
            last_states: HashMap::new(),
            session_rows: HashMap::new(),
            last_quota_exhausted: false,
            last_cost_cum: HashMap::new(),
            adapter_slow: HashMap::new(),
        }
    }

    /// 汇总各适配器的快轮信号（M2-1）：调度器快轮线程据此高频采样，
    /// 值变化才唤醒全量 tick——适配器各自声明，本层零 per-agent 分支
    pub fn hot_signals(&self) -> Vec<HotSignal> {
        self.adapters.iter().flat_map(|a| a.hot_signals()).collect()
    }

    /// 执行一轮采集+融合，返回岛快照
    pub fn tick(&mut self) -> IslandSnapshot {
        let now = now_ms();
        // 本轮计时与异常收集（tick 摘要输出，开发者模式排障主线索）
        let t0 = std::time::Instant::now();
        let mut had_error = false;
        let mut last_err: Option<String> = None;
        let mut hook_events_consumed = 0usize;
        let mut collect_stats: Vec<String> = vec![];

        // ① hooks 事件增量（M2-6/7 多 Agent 泛化）：按适配器逐家消费各自事件文件
        //    （events/<agent>.jsonl，桥脚本按 agent 落盘）；文件不存在 = 该家未安装
        //    增强档，静默降级（红线④）。结构：（agent id → （原始会话 id → 最新事件））
        let mut last_hooks: HashMap<&'static str, HashMap<String, (String, i64, Option<String>)>> =
            HashMap::new();
        for ad in &self.adapters {
            let agent_id = ad.id();
            let Some(path) = hook_events::events_file_path(agent_id) else { continue };
            let offset = self.hook_offsets.get(agent_id).copied().unwrap_or(0);
            match hook_events::read_events(&path, offset) {
                Ok((events, new_off)) => {
                    hook_events_consumed += events.len();
                    // 偏移无推进时免写库（R9，每 10s 一次的空写没必要）
                    if new_off != offset {
                        self.store.set_setting(
                            &format!("hook_events_offset:{agent_id}"),
                            &new_off.to_string(),
                        );
                    }
                    self.hook_offsets.insert(agent_id.to_string(), new_off);
                    for ev in events {
                        // 原始事件审计落库 status_events(02-DESIGN §3，R7)
                        self.store.insert_status_event(
                            agent_id,
                            Some(ev.session_id.as_str()),
                            &ev.hook,
                            &serde_json::to_string(&ev).unwrap_or_default(),
                            ev.ts,
                        );
                        // 每会话保留最新事件
                        last_hooks
                            .entry(agent_id)
                            .or_default()
                            .entry(ev.session_id.clone())
                            .and_modify(|e| {
                                if ev.ts >= e.1 {
                                    *e = (ev.hook.clone(), ev.ts, ev.message.clone());
                                }
                            })
                            .or_insert((ev.hook.clone(), ev.ts, ev.message.clone()));
                    }
                    // 轮转：仅当本轮已全部消费且超限；归零后回写偏移（审查 1.2）
                    let rotated = hook_events::rotate_if_large(
                        &path,
                        new_off,
                        hook_events::MAX_EVENT_FILE_BYTES,
                    );
                    if rotated != new_off {
                        self.hook_offsets.insert(agent_id.to_string(), rotated);
                        self.store.set_setting(&format!("hook_events_offset:{agent_id}"), "0");
                    }
                }
                Err(e) => {
                    log::warn!("[{agent_id}] hook 事件文件读取失败（本轮按无事件处理）：{e:#}");
                    had_error = true;
                    last_err = Some(format!("[{agent_id}] hook 事件读取失败：{e:#}"));
                }
            }
        }

        // ② 进程枚举兜底（L0：区分 idle 与 offline；30s 一探，结果缓存复用）。
        //    M2-4 声明化：匹配规则来自各适配器的 process_match，此处零 per-agent 分支
        if now - self.last_probe_ms >= PROBE_INTERVAL_MS {
            let proc_matches: Vec<(&'static str, Option<ProcessMatch>)> =
                self.adapters.iter().map(|a| (a.id(), a.process_match())).collect();
            let fresh = probe_processes(&mut self.sys, &proc_matches);
            // 存活翻转留痕：offline 误判/状态跳变排障的关键线索
            for (id, v) in &fresh {
                if self.probe_cache.get(id) != Some(v) {
                    log::debug!("[进程] {id} 存活探测翻转：{} → {}",
                        self.probe_cache.get(id).copied().unwrap_or(false), v);
                }
            }
            self.probe_cache = fresh;
            self.last_probe_ms = now;
        }

        // ③ 采集已勾选 Agent 的会话与用量（设置页 agents_enabled：勾选才采集/监控/展示，
        //    不勾选则完全不处理；设置键不存在时默认全部启用——兼容升级与首次运行）
        let enabled: Option<std::collections::HashSet<String>> = self
            .store
            .get_setting("agents_enabled")
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let is_enabled =
            |id: &str| match &enabled {
                Some(set) => set.contains(id),
                None => true,
            };

        let mut views: Vec<SessionView> = vec![];
        // 本轮全部实例扫到的会话 id（#18：消失检测统一在循环后判定）
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        // 同轮水位一致视图（2026-10-03 审查修复）：多根并存时同 id 多实例共享一个
        // 库水位键——若循环内逐实例现读，先跑实例采完即推高水位，同轮后跑实例（如
        // CODEX_HOME 重定向根）以新水位为下界，其停机期数据被永久滤掉（违反红线③
        // 「增量补录」）。本轮全部实例统一用 tick 开始时的水位快照，循环内照常写回
        let mut wm_snapshot: std::collections::HashMap<&'static str, i64> =
            std::collections::HashMap::new();
        for ad in &self.adapters {
            let agent_id = ad.id();
            if !is_enabled(agent_id) {
                // 禁用的适配器：清掉其会话观测缓存（用户主动关闭不算"会话消失"）
                self.last_states.retain(|k, _| !k.starts_with(agent_id));
                self.session_rows.retain(|k, _| !k.starts_with(agent_id));
                continue;
            }
            // 看门狗冷却（#17）：冷却期内跳过该适配器（其余 Agent 不受拖累）；
            // 冷却到期自动放行，下轮全量补采。该 Agent 会话本轮从快照缺位＝
            // 观测能力受损的可见降级（进入冷却那轮已计入 had_error）
            if let Some(&(_, until)) = self.adapter_slow.get(agent_id) {
                if now < until {
                    continue;
                }
            }
            let t_watch = std::time::Instant::now();
            let info_list = match ad.scan_sessions() {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("[{agent_id}] 会话扫描失败（本轮按空处理）：{e:#}");
                    had_error = true;
                    last_err = Some(format!("[{agent_id}] 会话扫描失败：{e:#}"));
                    vec![]
                }
            };
            // 会话 id 归集（#18：同一 Agent 可能多实例多根并存——如 CODEX_HOME 重
            // 定向＋默认根双份注册；"消失检测"必须在全部实例收集完后统一判定，
            // 逐实例判定会让 A 根实例把 B 根会话误判消失、互相清除观测缓存）
            for i in &info_list {
                seen_ids.insert(i.id.clone());
            }
            // 增量用量入库 + 水位推进（水位带 60s 安全余量，R4）；
            // 读 tick 开始时的快照（首个同 id 实例读库填充，后续实例复用）
            let watermark = match wm_snapshot.get(agent_id) {
                Some(&w) => w,
                None => {
                    let w = self.store.get_watermark(agent_id);
                    wm_snapshot.insert(agent_id, w);
                    w
                }
            }
            .saturating_sub(WATERMARK_MARGIN_MS);
            let output = match ad.collect_usage(watermark) {
                Ok(o) => o,
                Err(e) => {
                    log::warn!("[{agent_id}] 用量采集失败（本轮按空处理）：{e:#}");
                    had_error = true;
                    last_err = Some(format!("[{agent_id}] 用量采集失败：{e:#}"));
                    Default::default()
                }
            };
            let usage = output.rows;
            // 看门狗计时判定（#17）：scan＋collect 合计超阈值计一次"慢"；
            // 连续 3 轮进入冷却（进入那轮计入 had_error，degraded 可见）
            {
                let elapsed = t_watch.elapsed().as_millis();
                if elapsed > ADAPTER_SLOW_MS as u128 {
                    let entry = self
                        .adapter_slow
                        .entry(agent_id.to_string())
                        .or_insert((0, 0));
                    entry.0 += 1;
                    log::warn!(
                        "[{agent_id}] 本轮采集耗时 {elapsed}ms 超过 {}ms 阈值（连续第 {} 次）",
                        ADAPTER_SLOW_MS,
                        entry.0
                    );
                    if entry.0 >= ADAPTER_SLOW_STREAKS {
                        entry.1 = now + ADAPTER_COOLDOWN_MS;
                        log::warn!(
                            "[{agent_id}] 连续 {} 轮缓慢，冷却 {}s（其余 Agent 恢复正常节拍；恢复后自动补采）",
                            ADAPTER_SLOW_STREAKS,
                            ADAPTER_COOLDOWN_MS / 1000
                        );
                        entry.0 = 0;
                        had_error = true;
                        last_err = Some(format!("[{agent_id}] 采集持续缓慢，已进入 60s 冷却"));
                    }
                } else if let Some(entry) = self.adapter_slow.get_mut(agent_id) {
                    entry.0 = 0; // 恢复正常节拍，慢计数清零（冷却截止时间随到期自然失效）
                }
            }
            // 写库成败须显式感知（四轮审查红线③修复）：Err＝事务开启/提交失败，
            // 本批已回滚——下方水位必须跳过推进，否则早于「水位−60s」的行
            // 从此不再被采集＝静默永久丢数据；已成功场景靠幂等键下轮重采零副作用
            let insert_res = self.store.insert_usage(&usage);
            let inserted = insert_res.as_ref().copied().unwrap_or(0);
            // 会话标题（CC 转录 ai-title 行）：采集顺路带出，ZCode 恒空（session 表自带）
            let latest_titles: HashMap<String, String> = output.titles.into_iter().collect();
            // cost-state 校准（P5，2026-09-21）：CC 专属。会话级累计快照中超出
            // assistant 明细的部分是标题生成等后台调用的真实消耗——差值行入库，
            // token 计入消耗、调用次数不计。差值随 assistant 行增长而缩小，
            // 幂等键 'cost:{sid}:{model}' + 后台行无条件覆盖，每轮跟随重算值
            if !output.cost_snapshots.is_empty() {
                let calib_rows: Vec<UsageRow> = output
                    .cost_snapshots
                    .iter()
                    .filter(|snap| {
                        // 累计值未变化：跳过重算（省一次点查+写库）
                        self.last_cost_cum
                            .get(&(snap.session_id.clone(), snap.model.clone()))
                            != Some(&snap.cumulative_tokens)
                    })
                    .map(|snap| {
                        let assistant = self
                            .store
                            .assistant_model_total(&snap.session_id, &snap.model);
                        UsageRow {
                            session_id: snap.session_id.clone(),
                            agent: "claude-code".into(),
                            model: snap.model.clone(),
                            provider: provider_from_model(&snap.model),
                            ts: snap.ts,
                            input_tokens: Some((snap.cumulative_tokens - assistant).max(0)),
                            output_tokens: None,
                            reasoning_tokens: None,
                            cache_read_tokens: None,
                            cache_creation_tokens: None,
                            duration_ms: None,
                            ttft_ms: None,
                            error_type: None,
                            source_id: Some(format!(
                                "cost:{}:{}",
                                snap.session_id, snap.model
                            )),
                            is_background: true,
                        }
                    })
                    .collect();
                // 写库成功后才推进 last_cost_cum 缓存（四轮审查修复）：失败时保留
                // 旧累计值，下轮 filter 仍放行 → 重算重写，差值行不丢
                match self.store.insert_usage(&calib_rows) {
                    Ok(calib) => {
                        for snap in &output.cost_snapshots {
                            self.last_cost_cum.insert(
                                (snap.session_id.clone(), snap.model.clone()),
                                snap.cumulative_tokens,
                            );
                        }
                        if calib > 0 {
                            log::debug!(
                                "[校准] cost-state 后台差值行更新 {calib} 条（本轮快照 {} 组）",
                                output.cost_snapshots.len()
                            );
                        }
                    }
                    Err(_) => log::debug!("[校准] 差值行写库失败，缓存不推进（下轮重算重写）"),
                }
            }
            // 本适配器轮次统计（tick 摘要输出用）：采集行数/入库变更/水位推进一览
            collect_stats.push(format!(
                "{agent_id}：会话 {}，采到 {} 行入库 {inserted}",
                info_list.len(),
                usage.len()
            ));
            // 水位门控（四轮审查红线③修复）：写库失败（事务开启/提交失败）时本批
            // 已回滚，跳过推进——下轮从旧水位重采，幂等键保证零副作用；
            // 推进则本批早于「水位−60s」的行永久漏采＝静默丢数据
            if insert_res.is_ok() {
                if let Some(max_ts) = usage.iter().map(|u| u.ts).max() {
                    // 时钟防线（2026-09-21）：行时间戳为"未来值"（时钟回拨前写入/NTP 校正）
                    // 会把水位永久推高，之后所有新行被过滤——数据静默停更。钳制不超过当前时刻
                    self.store.set_watermark(agent_id, max_ts.min(now));
                }
            }
            // 本轮新采集中的最近错误（按会话）
            let mut recent_errors: HashMap<&str, i64> = HashMap::new();
            for u in &usage {
                if u.error_type.is_some() && u.ts > *recent_errors.get(u.session_id.as_str()).unwrap_or(&0) {
                    recent_errors.insert(u.session_id.as_str(), u.ts);
                }
            }

            // 批量取本批会话的用量拆解、最近模型与最近错误（审查 2.2.2:
            // 批量 GROUP BY/窗口函数替代逐会话点查——100 会话即每 10s 数百次无索引扫描）
            let ids: Vec<String> = info_list.iter().map(|i| i.id.clone()).collect();
            let breakdowns = self.store.session_usage_breakdown(&ids);
            let latest_models = self.store.latest_session_models(&ids);
            let latest_errors = self.store.latest_session_errors(&ids);
            // 已入库标题（持久层兜底）：快照不能依赖"本轮恰好采到新标题"
            let stored_titles = self.store.session_titles(&ids);
            // 每会话最近调用时间（CC 假 working 治理，用法见循环内注释）
            let latest_calls = self.store.latest_call_ts(&ids);

            for info in &info_list {
                let raw_sid = info.id.split_once(':').map(|(_, s)| s).unwrap_or("");
                // 启发式活动时间（M1-12 待议区「假 working」治理，2026-09-28）：
                // CC 的 scan 只有文件 mtime——ai-title/file-history 等非对话行追加
                // 也会推高 mtime，未装 hooks 时被 90s 活跃窗误判 working。改用自库
                // 最近调用时间覆盖（真实对话活动；本轮新行已先入库故含当轮），
                // 无调用行的会话为 None——诚实：无活动不冒充 working。装 hooks 的
                // 会话 last_hook 优先级更高不受影响；其余各家 last_usage_at 本就
                // 来自行时间戳/源库，维持原值。view 的「最近活动」同源覆盖，
                // 历史区时间排序一并去噪
                let activity_at = if agent_id == "claude-code" {
                    latest_calls.get(&info.id).copied()
                } else {
                    info.last_usage_at
                };
                let mut sig = SessionSignals {
                    last_activity_at: activity_at,
                    // 进程匹配是各 Agent 的私有知识：由适配器 process_match 声明（M2-4）
                    process_alive: self.probe_cache.get(agent_id).copied().unwrap_or(false),
                    ..Default::default()
                };
                // hooks 信号（M2-6/7 泛化）：装了 hooks 的家均可消费（事件里
                // session_id 为原始会话 id，与自库命名空间 id 的后缀对齐）
                if let Some(per_agent) = last_hooks.get(agent_id) {
                    if let Some((hook, ts, msg)) = per_agent.get(raw_sid) {
                        sig.last_hook = Some((hook.clone(), *ts));
                        sig.notification_message = msg.clone();
                        // 失败类事件（Kimi StopFailure/PostToolUseFailure）喂数据给
                        // 状态机 error 判定（04-EXPANSION §2.7 新信号位，比通知
                        // 文本启发式精确；CC 等无失败事件的家恒 None 不受影响）
                        if is_failure_event(hook) {
                            sig.last_failure = Some((*ts, hook.clone()));
                        }
                    }
                }
                if let Some(&ts) = recent_errors.get(info.id.as_str()) {
                    if now - ts <= ERROR_FRESH_MS {
                        sig.recent_error = Some((ts, "usage_error".into()));
                    }
                }
                let state = compute_state(&sig, now);
                // 首见/状态迁移留痕（"岛为什么变红/变琥珀"的时间线钥匙——
                // 状态机迁移是用户最直接感知的行为，埋点审查 2026-09-17 二次补强）
                match self.last_states.get(&info.id) {
                    None => log::debug!("[{agent_id}] 会话首见：{}（{state:?}）", info.id),
                    Some(prev) if *prev != state => {
                        log::debug!("[{agent_id}] 会话 {} 状态迁移：{prev:?} → {state:?}", info.id)
                    }
                    _ => {}
                }
                self.last_states.insert(info.id.clone(), state);
                // 标题：scan 阶段（ZCode）自带；CC 转录标题由本次采集带出兜底
                let title = info
                    .title
                    .clone()
                    .or_else(|| latest_titles.get(&info.id).cloned());
                // 模型：CC scan 阶段拿不到（转录文件级无模型信息），用量流水最近一次回填——
                // 否则 sessions.model 恒 NULL，会话窗口模型列永远显示 —（岛面板有回填所以正确，
                // 两处口径不一致；回填后所有读 sessions 表的展示位统一）
                let model = info
                    .model
                    .clone()
                    .or_else(|| latest_models.get(&info.id).cloned());
                // 会话元数据与状态入库（收缩态查询走内存快照，库做持久层）；
                // 写入去重（2026-10-03 审查优化）：签名与上次入库一致则跳过——
                // COALESCE 字段按「新值 Some 时才可能变化」对齐 SQL 生效语义，
                // last_seen_at 按 MAX 单调语义取较大者。签名缓存未命中（首见/
                // 重启首轮）必然走一次真实 upsert
                let state_str =
                    serde_json::to_string(&state).unwrap_or_default().trim_matches('"').to_string();
                let effective = |cached: &Option<String>, new: &Option<String>| -> Option<String> {
                    match (cached, new) {
                        // 新值为 None 时库不覆盖，生效值仍是缓存值
                        (prev, None) => prev.clone(),
                        (prev, Some(v)) if prev.as_deref() == Some(v.as_str()) => prev.clone(),
                        (_, Some(v)) => Some(v.clone()),
                    }
                };
                let changed = match self.session_rows.get(&info.id) {
                    Some(prev) => {
                        let next = SessionRowSig {
                            provider: effective(&prev.provider, &info.provider),
                            model: effective(&prev.model, &model),
                            project_dir: effective(&prev.project_dir, &info.project_dir),
                            title: effective(&prev.title, &title),
                            last_seen_at: prev.last_seen_at.max(info.last_seen_at),
                            state: state_str.clone(),
                        };
                        &next != prev
                    }
                    None => true,
                };
                if changed
                    && self
                        .store
                        .upsert_session(
                            &info.id, agent_id,
                            info.provider.as_deref(), model.as_deref(),
                            info.project_dir.as_deref(), title.as_deref(),
                            info.last_seen_at,
                            &state_str,
                            None,
                        )
                        .is_ok()
                {
                    // 仅写库成功后才更新签名缓存（四轮审查落实）：upsert 返回 Err
                    // （写库失败，store 层已 warn）时保留旧签名，下轮 changed 仍为
                    // true 重写——若照样推进缓存会让该行持续缺写直到字段变化
                    self.session_rows.insert(
                        info.id.clone(),
                        SessionRowSig {
                            provider: info.provider.clone(),
                            model: model.clone(),
                            project_dir: info.project_dir.clone(),
                            title: title.clone(),
                            last_seen_at: info.last_seen_at,
                            state: state_str,
                        },
                    );
                }
                // 用量拆解：总消耗 = 四项相加（账单口径，含缓存）
                let bd = breakdowns.get(&info.id).cloned().unwrap_or_default();
                views.push(SessionView {
                    id: info.id.clone(),
                    agent: agent_id.to_string(),
                    // Claude Code scan 阶段拿不到 model，从自库最近一次调用兜底回填（R1）
                    model: info.model.clone().or_else(|| latest_models.get(&info.id).cloned()),
                    project_dir: info.project_dir.clone(),
                    // 标题三级来源：scan 自带（ZCode）> 本轮新采集（CC）> 已入库持久值
                    title: info
                        .title
                        .clone()
                        .or_else(|| latest_titles.get(&info.id).cloned())
                        .or_else(|| stored_titles.get(&info.id).cloned()),
                    state,
                    session_tokens: bd.total(),
                    input_tokens: bd.input,
                    output_tokens: bd.output,
                    cache_read_tokens: bd.cache_read,
                    cache_creation_tokens: bd.cache_creation,
                    error_type: latest_errors.get(&info.id).map(|(e, _)| e.clone()),
                    // 与状态机同源（CC＝自库最近调用，其余＝scan 值），见循环内注释
                    last_activity_at: activity_at,
                });
            }
        }

        // 会话消失检测（统一判定，#18 从循环内迁出）：上轮有、本轮全体实例皆无
        // （90 天 cutoff / 截断 / 会话结束都会导致）——"会话不见了"报障的时间线
        // 线索（埋点审查 2026-09-17 二次补强；多实例 Agent 必须以并集判定）
        let gone: Vec<String> = self
            .last_states
            .keys()
            .filter(|k| !seen_ids.contains(k.as_str()))
            .cloned()
            .collect();
        for id in &gone {
            self.last_states.remove(id);
            self.session_rows.remove(id);
            log::debug!("[聚合] 会话从观测列表消失：{id}");
        }

        // ③′＋④ 供应商实例热刷新与到期外呼：已整体迁至 provider/worker.rs
        //    独立线程（2026-09-29 审查修复）——fetch 阻塞不再绑架聚合 tick，
        //    本线程只经 SQLite 读最新快照组装视图（下方 ⑤ 段）

        // ⑤ 额度耗尽检测（批次三 F2 泛化）：任一启用实例的任一窗口用量 ≥100%
        // 即触发岛红态——原 M2 时代仅判 glm/5h，claude-local 等窗口口径实例
        // 跑满不会变红；数据源改用启用实例集合（island_accounts），停用实例
        // 的陈旧快照不再误报。只产出快照级标志位，不改写会话状态——
        // 会话状态被统一改成 Error 会让贴边标签的分段全变一色，丢失 Agent 区分度；
        // 额度告警由前端表达（胶囊变红/标签红光/额度弧线红）
        let quotas = self.store.latest_quotas();
        let accounts_view = island_accounts(&self.store, &quotas);
        let quota_exhausted = quota_exhausted(&accounts_view);
        // 耗尽标志翻转留痕（前端红光状态的时间线）
        if quota_exhausted != self.last_quota_exhausted {
            log::debug!(
                "[额度] 5h 耗尽标志翻转：{} → {}",
                self.last_quota_exhausted,
                quota_exhausted
            );
            self.last_quota_exhausted = quota_exhausted;
        }

        let island = aggregate(&views.iter().map(|v| v.state).collect::<Vec<_>>());
        let quota_views = quotas.iter().map(QuotaView::from).collect();
        // degraded：连续多轮有采集源失败才置位；恢复即清零。
        // 进入/恢复各留痕一次（不再 degraded 期间每轮重复刷 warn），
        // 进入时带上最近错误根因（2026-09-17 埋点审查）
        if had_error {
            self.fail_streak += 1;
            self.last_error = last_err;
        } else {
            if self.fail_streak >= DEGRADE_AFTER_TICKS {
                log::info!("采集已恢复正常，degraded 解除（此前连续失败 {} 轮）", self.fail_streak);
            }
            self.fail_streak = 0;
            self.last_error = None;
        }
        let degraded = self.fail_streak >= DEGRADE_AFTER_TICKS;
        if self.fail_streak == DEGRADE_AFTER_TICKS {
            log::warn!(
                "采集连续 {} 轮失败，快照标记 degraded（最近错误：{}）",
                self.fail_streak,
                self.last_error.as_deref().unwrap_or("未知")
            );
        }
        // tick 摘要（开发者模式排障主线索：每轮一条，产出/耗时/降级状态一览；
        // 正常态约 1MB/天内，按天滚动设计完全接得住）
        log::debug!(
            "[聚合] tick 耗时 {}ms：{}；hook 事件 {} 条；degraded={}",
            t0.elapsed().as_millis(),
            if collect_stats.is_empty() {
                "无启用的适配器".to_string()
            } else {
                collect_stats.join("；")
            },
            hook_events_consumed,
            degraded
        );
        // 今日口径（展示改造 2026-09-18）：本机今日零点起算，给胶囊/面板的
        // "今日消耗"展示——比全历史"累计"更贴近用户心智
        let (today_tokens, today_calls) = self.store.today_usage(today_start_ms());
        IslandSnapshot {
            sessions: views,
            island,
            quotas: quota_views,
            // 供应商实例视图（M3-6）：岛轮播/托盘归一的共用数据源（注册表投影）；
            // 批次三起 quota_exhausted 与此同源（上方已算好，避免二次投影）
            accounts: accounts_view,
            quota_exhausted,
            degraded,
            today_tokens,
            today_calls,
            generated_at: now,
        }
    }
}

/// 本机今日零点的 Unix 毫秒（chrono 本地时区；解析异常兜底为 0 = 全历史口径）
fn today_start_ms() -> i64 {
    chrono::Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(chrono::Local).single())
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}

/// 额度耗尽判定（批次三 F2 泛化后提纯为纯函数）：任一启用实例的任一窗口
/// 用量 ≥100% 即真。原 M2 时代仅判 glm/5h 字面量，claude-local 等窗口口径
/// 实例跑满不会触发岛红态；提纯便于单测穷举各窗口口径
fn quota_exhausted(accounts: &[IslandAccountView]) -> bool {
    accounts
        .iter()
        .any(|a| a.quotas.iter().any(|q| q.used_percent >= Some(100.0)))
}

/// 进程枚举：按各适配器声明的 ProcessMatch 判存活（M2-4 声明化，
/// 原 zcode/claude 硬编码匹配已迁入各自适配器）。
/// sys 由调用方持有复用（审查 2.2.4：Windows 上全量刷新进程含命令行读取，开销可观）
fn probe_processes(
    sys: &mut sysinfo::System,
    matches: &[(&'static str, Option<ProcessMatch>)],
) -> HashMap<String, bool> {
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut alive: HashMap<String, bool> =
        matches.iter().map(|(id, _)| (id.to_string(), false)).collect();
    'outer: for (_, proc) in sys.processes() {
        let n = proc.name().to_string_lossy().to_ascii_lowercase();
        let cmd = proc
            .cmd()
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<String>()
            .to_ascii_lowercase();
        for (id, pm) in matches {
            if alive.get(*id) == Some(&true) {
                continue; // 已命中：无需重复判定
            }
            let Some(pm) = pm else { continue };
            let hit = pm.name_keywords.iter().any(|k| n.contains(k))
                || pm.cmd_keywords.iter().any(|k| cmd.contains(k));
            let excluded = pm.cmd_excludes.iter().any(|k| cmd.contains(k));
            if hit && !excluded {
                alive.insert(id.to_string(), true);
            }
        }
        if alive.values().all(|&v| v) {
            break 'outer; // 全部命中：提前收工
        }
    }
    alive
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

    // 退避阶梯与实例表指纹测试已随调度逻辑迁至 provider/worker.rs（2026-09-29）

    /// 实例视图组装（M3-6）：停用实例过滤／快照按实例归属分组／注册表投影
    /// （名称/品牌色/口径）／未知厂商兜底不阻塞
    #[test]
    fn test_island_accounts() {
        let mut db = std::env::temp_dir();
        db.push(format!("at-m36-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = Store::open(&db).unwrap();
        let mk = |id: &str, kind: &str, alias: &str, enabled: bool, in_island: bool| {
            crate::store::ProviderAccount {
                id: id.into(),
                kind_id: kind.into(),
                alias: alias.into(),
                base_override: None,
                cred_kind: "plain".into(),
                cred_value: None,
                note: None,
                enabled,
                in_island,
                origin: "manual".into(),
                created_at: 0,
                updated_at: 0,
            }
        };
        store.insert_provider_account(&mk("acc-g1", "glm", "主号", true, true)).unwrap();
        store.insert_provider_account(&mk("acc-g2", "glm", "副号", true, false)).unwrap();
        store.insert_provider_account(&mk("acc-ds", "deepseek", "个人号", true, true)).unwrap();
        store.insert_provider_account(&mk("acc-off", "glm", "停用", false, true)).unwrap();
        // 窗口快照分属两个 glm 实例（验证按 account_id 分组不串号）
        store.insert_quota(&crate::store::QuotaRow {
            provider: "glm".into(),
            account_id: Some("acc-g1".into()),
            window_kind: "5h".into(),
            used_percent: Some(12.0),
            used_tokens: None,
            reset_at: None,
            fetched_at: 1000,
        });
        store.insert_quota(&crate::store::QuotaRow {
            provider: "glm".into(),
            account_id: Some("acc-g2".into()),
            window_kind: "weekly".into(),
            used_percent: Some(45.0),
            used_tokens: None,
            reset_at: None,
            fetched_at: 1000,
        });
        store.insert_balance(&crate::store::BalanceRow {
            account_id: "acc-ds".into(),
            currency: "CNY".into(),
            total: 6.20,
            granted: None,
            available: None,
            fetched_at: 2000,
        });
        let views = island_accounts(&store, &store.latest_quotas());
        assert_eq!(views.len(), 3, "停用实例不入快照");
        let g1 = views.iter().find(|v| v.id == "acc-g1").unwrap();
        assert_eq!(g1.quotas.len(), 1, "窗口快照按实例归属分组");
        assert_eq!(g1.quotas[0].window_kind, "5h");
        assert!(g1.in_island);
        assert_eq!(g1.kind_name, "GLM", "厂商名为注册表投影（2026-10-10 定名）");
        assert_eq!(g1.color, "#2f6bff");
        assert_eq!(g1.quota_kind, "windows");
        let g2 = views.iter().find(|v| v.id == "acc-g2").unwrap();
        assert!(!g2.in_island, "展示集标记透传（岛端过滤依据）");
        assert_eq!(g2.quotas[0].window_kind, "weekly");
        let ds = views.iter().find(|v| v.id == "acc-ds").unwrap();
        assert_eq!(ds.quota_kind, "balance");
        let bal = ds.balance.as_ref().expect("余额快照应随视图下发");
        assert_eq!(bal.total, 6.20);
        assert_eq!(bal.fetched_at, 2000);
        // 未知厂商：注册表缺该厂商时兜底 kind_id＋中性色，不阻塞快照
        store.insert_provider_account(&mk("acc-x", "no-such-kind", "兜底", true, true)).unwrap();
        let views = island_accounts(&store, &store.latest_quotas());
        let x = views.iter().find(|v| v.id == "acc-x").unwrap();
        assert_eq!(x.kind_name, "no-such-kind");
        assert_eq!(x.quota_kind, "");
        assert_eq!(x.color, "#8a8f98");
        let _ = std::fs::remove_file(&db);
    }

    /// 批次三 F2：额度耗尽判定泛化——任一实例任一窗口 ≥100% 即触发，
    /// glm/5h 字面量不再特殊（修复 claude-local 等窗口口径跑满不变红）
    #[test]
    fn test_quota_exhausted_any_provider_any_window() {
        let q = |kind: &str, pct: Option<f64>| QuotaView {
            provider: "p".into(),
            account_id: None,
            window_kind: kind.into(),
            used_percent: pct,
            reset_at: None,
            fetched_at: 1000,
        };
        let acc = |quotas: Vec<QuotaView>| IslandAccountView {
            id: "a".into(),
            kind_id: "k".into(),
            kind_name: "n".into(),
            color: "#000000".into(),
            quota_kind: "windows".into(),
            alias: "a".into(),
            in_island: true,
            quotas,
            balance: None,
        };
        // claude-local 的 weekly 跑满同样触发（旧实现漏报的主场景）
        assert!(quota_exhausted(&[acc(vec![q("weekly", Some(100.0))])]));
        assert!(quota_exhausted(&[acc(vec![q("5h", Some(64.0)), q("weekly", Some(100.0))])]));
        // 99% / 无数据 / 空集合不触发
        assert!(!quota_exhausted(&[acc(vec![q("5h", Some(99.0))])]));
        assert!(!quota_exhausted(&[acc(vec![q("5h", None)])]));
        assert!(!quota_exhausted(&[]));
    }

    /// e2e：本机真实数据跑两轮 tick（手动：cargo test -- --ignored test_real_aggregator）
    #[test]
    #[ignore]
    fn test_real_aggregator() {
        let mut db = std::env::temp_dir();
        db.push(format!("at-t7-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = Arc::new(Store::open(&db).unwrap());
        let mut agg = Aggregator::new(store.clone());

        let snap1 = agg.tick();
        println!(
            "tick1：会话 {} 个 | 岛 {:?} | 额度 {:?}",
            snap1.sessions.len(),
            snap1.island,
            snap1.quotas.iter().map(|q| format!("{}:{}%", q.window_kind, q.used_percent.unwrap_or(0.0) as i32)).collect::<Vec<_>>()
        );
        // 本机双 Agent 都在运行，必有会话；状态必须合法
        assert!(!snap1.sessions.is_empty(), "本机应有活跃会话");
        assert!(snap1.sessions.iter().any(|s| s.agent == "zcode"), "应含 zcode 会话");
        assert!(snap1.sessions.iter().any(|s| s.agent == "claude-code"), "应含 claude-code 会话");
        // ZCode 当前会话在写数据 → 应为 working 或 error（额度 100% 时标红）
        let zc = snap1.sessions.iter().find(|s| s.agent == "zcode").unwrap();
        println!("zcode 会话：state={:?} tokens={}", zc.state, zc.session_tokens);
        assert!(zc.session_tokens > 0, "本会话应有 token 统计");

        // 第二轮：水位增量幂等（会话 token 不变或仅微增）
        let snap2 = agg.tick();
        assert_eq!(snap2.sessions.len(), snap1.sessions.len());
        let _ = std::fs::remove_file(&db);
    }
}
