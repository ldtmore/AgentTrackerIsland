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

use crate::collector::claude_code::ClaudeCodeAdapter;
use crate::collector::codex::CodexAdapter;
use crate::collector::engine::{HotSignal, ProcessMatch};
use crate::collector::gemini::GeminiAdapter;
use crate::collector::hook_events;
use crate::collector::kimi::KimiCodeAdapter;
use crate::collector::openclaw::OpenClawAdapter;
use crate::collector::opencode::OpenCodeFamilyAdapter;
use crate::collector::qwen::QwenCodeAdapter;
use crate::collector::zcode::ZcodeAdapter;
use crate::collector::{AgentAdapter, provider_from_model};
use crate::store::UsageRow;
use crate::provider::glm::GlmProvider;
use crate::provider::ProviderAdapter;
use crate::state::{
    aggregate, compute_state, is_failure_event, ERROR_FRESH_MS, IslandState, SessionSignals,
    SessionState,
};
use crate::store::Store;

/// 额度刷新间隔（毫秒）
const QUOTA_REFRESH_MS: i64 = 5 * 60 * 1000;
/// 水位安全余量（毫秒，R4）：并发会话慢刷盘的行 ts 可能略小于其他会话推进的
/// 全局水位，按原始水位过滤会永久丢行；回退 60s 重采，幂等键保证不重复入库
const WATERMARK_MARGIN_MS: i64 = 60 * 1000;
/// 进程枚举间隔（毫秒，审查 2.2.4）：全量刷新开销可观，且进程存亡只影响
/// idle/offline 粒度，30s 精度足够
const PROBE_INTERVAL_MS: i64 = 30 * 1000;
/// 连续失败多少轮判定 degraded（10s/轮，3 轮 ≈ 30s：瞬时抖动不闪红）
const DEGRADE_AFTER_TICKS: u32 = 3;

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

/// 展示用额度视图
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuotaView {
    pub provider: String,
    pub window_kind: String,
    pub used_percent: Option<f64>,
    pub reset_at: Option<i64>,
}

/// 岛快照：一次 tick 的完整产出。
/// 2026-09-18 展示改造：新增今日口径（today_*）与额度配置标志（glm_configured，
/// 供胶囊区分"未配置"与"查询失败"，替代额度段凭空消失）
#[derive(Debug, Clone, serde::Serialize)]
pub struct IslandSnapshot {
    pub sessions: Vec<SessionView>,
    pub island: IslandState,
    pub quotas: Vec<QuotaView>,
    /// GLM 5h 额度已耗尽（100%）：不改写会话状态，由前端驱动胶囊变红/标签红光
    pub quota_exhausted: bool,
    /// 采集源连续失败（审查 1.1）：前端在收缩态提示"采集异常"，
    /// 与"没有会话"区分开——降级必须可见，不允许静默失明
    pub degraded: bool,
    /// 今日 token 总量（本机今日零点起，账单口径含缓存）
    pub today_tokens: i64,
    /// 今日模型调用次数（本机今日零点起）
    pub today_calls: i64,
    /// GLM 凭据是否已配置（false 时前端展示"额度未配置"而非不展示）
    pub glm_configured: bool,
    pub generated_at: i64,
}

/// 聚合器（有状态：水位/事件偏移/额度刷新计时/进程探测缓存）
pub struct Aggregator {
    store: Arc<Store>,
    /// Agent 适配器注册表（审查 3.6）：新增 Agent = 实现 trait 后在此登记，
    /// 采集主循环不出现 per-agent if-else
    adapters: Vec<Box<dyn AgentAdapter>>,
    /// hook 事件文件的消费偏移（M2-6/7 多 Agent 泛化）：agent id → 偏移；
    /// 持久化于 app_settings 键 `hook_events_offset:<agent>`
    hook_offsets: HashMap<String, u64>,
    /// 上次额度刷新成功时间
    last_quota_fetch: i64,
    glm: Option<GlmProvider>,
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
    /// 上轮额度耗尽标志（翻转留痕用）
    last_quota_exhausted: bool,
    /// cost-state 校准缓存：会话×模型 → 上次入库的累计值（值未变则跳过重算；
    /// 内存态，重启后首轮全量重算一遍，幂等无害）
    last_cost_cum: HashMap<(String, String), i64>,
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
        // GLM 凭据优先级：应用设置（token 非空才生效）> 环境变量 > claude-menu
        // suppliers.json；设置页"留空则继续沿用"= token 为空时回落自动发现链
        let (glm, source) = match (
            store.get_setting("glm_base"),
            store.get_setting("glm_token"),
        ) {
            (Some(base), Some(token)) if !base.is_empty() && !token.is_empty() => {
                (Some(GlmProvider::new(&base, &token)), "应用设置".to_string())
            }
            _ => match GlmProvider::discover() {
                Some(c) => (Some(GlmProvider::new(&c.base, &c.token)), c.source.to_string()),
                None => (None, String::new()),
            },
        };
        // 记录凭据来源供设置页展示"（当前：xxx）"；无凭据时清除旧记录
        store.set_setting("glm_token_source", &source);
        if glm.is_none() {
            log::info!("GLM 凭据未配置：额度功能区停用（设置页可配置，或走自动发现链）");
        } else {
            // 凭据来源留痕（debug）：额度查不到时先看走的是哪条链、哪个平台
            log::debug!(
                "[额度] 凭据来源：{source}，base={}",
                glm.as_ref().map(|g| g.base()).unwrap_or("")
            );
        }
        // 0003 重建流程（2026-09-21 数据准确性治理）：迁移已清空 usage_records，
        // 此处归零水位并摘除标志——首轮 collect_usage(0) 全量回溯自动重建
        // （数据源 CC 转录/ZCode 源库都是完整事实源，重建零损失）
        if store.get_setting(crate::store::REBUILD_PENDING_KEY).as_deref() == Some("1") {
            store.clear_watermarks();
            store.set_setting(crate::store::REBUILD_PENDING_KEY, "0");
            log::info!("[重建] 幂等键升级（0003）：水位已归零，首轮采集将全量回溯重建用量数据");
        }
        Self {
            store,
            adapters: vec![
                Box::new(ZcodeAdapter::new()),
                Box::new(ClaudeCodeAdapter::new()),
                Box::new(CodexAdapter::new()),
                Box::new(KimiCodeAdapter::new()),
                // OpenCode 同族两实例（M2-10a）：同一 struct 不同 FamilyProfile，
                // 差异仅 agent id/数据根/db 文件名（01-RESEARCH §12）
                Box::new(OpenCodeFamilyAdapter::opencode()),
                Box::new(OpenCodeFamilyAdapter::mimo_code()),
                // Gemini CLI＋Qwen Code（M2-11）：fork 已分叉故各自独立适配器
                // （调研推翻「同族参数化换根即用」预想，01-RESEARCH §13）
                Box::new(GeminiAdapter::new()),
                Box::new(QwenCodeAdapter::new()),
                // OpenClaw（M2-13）：多 agent 多库（每 agentId 一库，目录枚举）
                Box::new(OpenClawAdapter::new()),
            ],
            hook_offsets,
            last_quota_fetch: 0,
            glm,
            sys: sysinfo::System::new(),
            probe_cache: HashMap::new(),
            last_probe_ms: 0,
            fail_streak: 0,
            last_error: None,
            last_states: HashMap::new(),
            last_quota_exhausted: false,
            last_cost_cum: HashMap::new(),
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
        for ad in &self.adapters {
            let agent_id = ad.id();
            if !is_enabled(agent_id) {
                // 禁用的适配器：清掉其会话观测缓存（用户主动关闭不算"会话消失"）
                self.last_states.retain(|k, _| !k.starts_with(agent_id));
                continue;
            }
            let info_list = match ad.scan_sessions() {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("[{agent_id}] 会话扫描失败（本轮按空处理）：{e:#}");
                    had_error = true;
                    last_err = Some(format!("[{agent_id}] 会话扫描失败：{e:#}"));
                    vec![]
                }
            };
            // 会话消失检测：上轮有、本轮无（90 天 cutoff / 截断 / 会话结束都会导致）——
            // "会话不见了"报障的时间线线索（埋点审查 2026-09-17 二次补强）
            let current_ids: std::collections::HashSet<&str> =
                info_list.iter().map(|i| i.id.as_str()).collect();
            let gone: Vec<String> = self
                .last_states
                .keys()
                .filter(|k| k.starts_with(agent_id) && !current_ids.contains(k.as_str()))
                .cloned()
                .collect();
            for id in &gone {
                self.last_states.remove(id);
                log::debug!("[{agent_id}] 会话从观测列表消失：{id}");
            }
            // 增量用量入库 + 水位推进（水位带 60s 安全余量，R4）
            let watermark = self
                .store
                .get_watermark(agent_id)
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
            let inserted = self.store.insert_usage(&usage);
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
                for snap in &output.cost_snapshots {
                    self.last_cost_cum
                        .insert((snap.session_id.clone(), snap.model.clone()), snap.cumulative_tokens);
                }
                let calib = self.store.insert_usage(&calib_rows);
                if calib > 0 {
                    log::debug!(
                        "[校准] cost-state 后台差值行更新 {calib} 条（本轮快照 {} 组）",
                        output.cost_snapshots.len()
                    );
                }
            }
            // 本适配器轮次统计（tick 摘要输出用）：采集行数/入库变更/水位推进一览
            collect_stats.push(format!(
                "{agent_id}：会话 {}，采到 {} 行入库 {inserted}",
                info_list.len(),
                usage.len()
            ));
            if let Some(max_ts) = usage.iter().map(|u| u.ts).max() {
                // 时钟防线（2026-09-21）：行时间戳为"未来值"（时钟回拨前写入/NTP 校正）
                // 会把水位永久推高，之后所有新行被过滤——数据静默停更。钳制不超过当前时刻
                self.store.set_watermark(agent_id, max_ts.min(now));
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

            for info in &info_list {
                let raw_sid = info.id.split_once(':').map(|(_, s)| s).unwrap_or("");
                let mut sig = SessionSignals {
                    last_activity_at: info.last_usage_at,
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
                // 会话元数据与状态入库（收缩态查询走内存快照，库做持久层）
                self.store.upsert_session(
                    &info.id, agent_id,
                    info.provider.as_deref(), model.as_deref(),
                    info.project_dir.as_deref(), title.as_deref(),
                    info.last_seen_at,
                    &serde_json::to_string(&state).unwrap_or_default().trim_matches('"').to_string(),
                    None,
                );
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
                    last_activity_at: info.last_usage_at,
                });
            }
        }

        // ④ GLM 额度：按间隔刷新，失败降级（最近快照仍可用）。
        //    额度失败有独立降级语义（glm.rs 已留痕），不计入 degraded——
        //    degraded 表达"观测能力受损"，而非"外部接口抖动"
        if now - self.last_quota_fetch >= QUOTA_REFRESH_MS {
            if let Some(glm) = &self.glm {
                if let Ok(rows) = glm.fetch_quota() {
                    for r in &rows {
                        self.store.insert_quota(r);
                    }
                    self.last_quota_fetch = now;
                }
            }
        }

        // ⑤ 额度耗尽检测（5h 窗口 100%）：只产出快照级标志位，不改写会话状态——
        // 会话状态被统一改成 Error 会让贴边标签的分段全变一色，丢失 Agent 区分度；
        // 额度告警由前端表达（胶囊变红/标签红光/额度弧线红）
        let quotas = self.store.latest_quotas();
        let quota_exhausted = quotas
            .iter()
            .any(|q| q.provider == "glm" && q.window_kind == "5h" && q.used_percent >= Some(100.0));
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
        let quota_views = quotas
            .into_iter()
            .map(|q| QuotaView { provider: q.provider, window_kind: q.window_kind, used_percent: q.used_percent, reset_at: q.reset_at })
            .collect();
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
            quota_exhausted,
            degraded,
            today_tokens,
            today_calls,
            glm_configured: self.glm.is_some(),
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
