//! 供应商额度调度工作线程（2026-09-29 审查修复，自 state/service.rs tick ④ 段
//! 拆出独立线程）：blocking fetch（5s 超时）与启动错峰 sleep 只发生在本线程，
//! N 实例同时到期且全部超时也不再绑架聚合 tick 的 island-snapshot 广播
//! （原实现单轮 tick 可阻塞 N×5s，会话状态与岛刷新随之停摆）。
//! 职责（低频维护线程定位）：
//!   ① 每实例独立水位到期外呼＋指数退避（5→10→20→封顶 60min，06-PLAN §5）；
//!   ② 实例表指纹热刷新（M3-4：设置页增删改/启停 1~2 个轮询周期内重装载）；
//!   ③ 手动刷新通知复位退避（额度页单卡「查询」成功后，调度器按 5min 周期
//!     接力，不再按旧退避原地再查一遍）；
//!   ④ 每日一次数据清理节拍（原仅启动时清理，常驻不重启场景库无限增长）。
//! 线程安全：quota_sched/adapters/sig 全部为本线程独占（非 Sync 的
//! Box<dyn ProviderAdapter> 也不需要跨线程共享）；与聚合 tick 的数据交换
//! 只经 SQLite（fetch 落库 → tick 读 latest_quotas 组装快照），无共享内存。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use super::{bootstrap, ProviderAdapter, QuotaSnapshot};
use crate::store::Store;

/// 额度刷新间隔（毫秒）
const QUOTA_REFRESH_MS: i64 = 5 * 60 * 1000;
/// 额度失败指数退避台阶（分钟，06-PLAN §5）：连续失败按 5→10→20→上限 60，
/// 成功即复位；退避中不影响其他实例
const QUOTA_BACKOFF_STEPS_MIN: [i64; 4] = [5, 10, 20, 60];
/// 启动风暴间隔（毫秒，06-PLAN §5）：应用启动首轮，首次尝试的实例之间 ≥2s，
/// 避免 N 家同时外呼
const QUOTA_STARTUP_STAGGER_MS: i64 = 2 * 1000;
/// 工作线程轮询周期（毫秒）：到期判定粒度（5min 周期下 2s 粒度足够；
/// 每轮纯内存比对＋一次实例表小查询，成本可忽略）
const WORKER_POLL_MS: u64 = 2 * 1000;
/// 数据清理节拍（毫秒）：与启动清理同款逻辑，常驻不重启场景每日补一次
const CLEANUP_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

/// 单实例额度调度状态（内存水位，06-PLAN §5：重启后首轮全查一遍可接受，不新增表）
#[derive(Default)]
struct QuotaSched {
    /// 上次尝试（无论成败）时刻；0 = 从未（启动首轮到期）
    last_attempt_ms: i64,
    /// 连续失败次数（指数退避输入，成功即复位）
    fail_streak: u32,
}

/// 失败退避间隔（毫秒）：连续失败 1/2/3 次按 5/10/20min，≥4 次封顶 60min。
/// 纯函数（单测锁定阶梯与封顶）
fn quota_backoff_ms(fail_streak: u32) -> i64 {
    let idx = (fail_streak.max(1) as usize - 1).min(QUOTA_BACKOFF_STEPS_MIN.len() - 1);
    QUOTA_BACKOFF_STEPS_MIN[idx] * 60 * 1000
}

/// 实例表指纹（M3-4 热刷新）：id＋updated_at 拼接。增删改/启停/展示集切换
/// 都会刷新 updated_at，签名随之变化。纯函数（单测锁定）
pub(crate) fn provider_sig(accounts: &[crate::store::ProviderAccount]) -> String {
    let mut parts: Vec<String> = accounts
        .iter()
        .map(|a| format!("{}:{}", a.id, a.updated_at))
        .collect();
    parts.sort(); // 顺序无关（list 输出序本就稳定，排序防御未来排序规则变化）
    parts.join("|")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 按设置周期清理过期数据（0=永不；未设置默认保留 1 年）。启动时主线程与
/// 本线程每日节拍共用同一实现，返回删除条数（调用方自行留痕）
pub(crate) fn run_scheduled_cleanup(store: &Store) -> u64 {
    let cleanup_days = store
        .get_setting("cleanup_days")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(365);
    if cleanup_days <= 0 {
        return 0;
    }
    let cutoff =
        now_ms() - cleanup_days * 86_400_000;
    store.cleanup_older_than(cutoff)
}

/// 额度调度主循环体（catch_unwind 的载荷）：一轮＝通知排空→清理节拍→
/// 热刷新→到期外呼。返回是否发生 panic 由外层 catch 判定
fn run_once(
    store: &Arc<Store>,
    adapters: &mut Vec<Box<dyn ProviderAdapter>>,
    sig: &mut String,
    sched: &mut HashMap<String, QuotaSched>,
    last_cleanup_ms: &mut i64,
    notify: &Receiver<String>,
) {
    let now = now_ms();
    // ① 手动刷新通知：额度页「查询」已真实落库，复位该实例退避并按 5min
    //    周期接力（通知幂等：不存在该实例的条目时忽略）
    while let Ok(id) = notify.try_recv() {
        if let Some(s) = sched.get_mut(&id) {
            s.fail_streak = 0;
            s.last_attempt_ms = now;
        }
    }
    // ② 每日清理节拍（首拍从启动后 24h 计；启动清理由主线程先执行一次）
    if now - *last_cleanup_ms >= CLEANUP_INTERVAL_MS {
        let removed = run_scheduled_cleanup(store);
        if removed > 0 {
            log::info!("[维护] 每日清理：删除 {removed} 条过期数据");
        }
        *last_cleanup_ms = now;
    }
    // ③ 实例表热刷新（M3-4）：指纹与上轮不同才重建（迁移防重入标志使
    //    load_instances 幂等；发现器只补零实例厂商）；水位表 retain 清理
    //    已消失实例、保留在役实例的退避进度
    let cur_sig = provider_sig(&store.list_provider_accounts());
    if cur_sig != *sig {
        log::info!(
            "[额度] 实例表变更，重新装载供应商适配器（{} 个）",
            store.list_provider_accounts().len()
        );
        *adapters = bootstrap::load_instances(store.clone());
        let alive: std::collections::HashSet<String> =
            adapters.iter().map(|a| a.account_id().to_string()).collect();
        sched.retain(|id, _| alive.contains(id));
        *sig = cur_sig;
    }
    // ④ 到期外呼：全局开关 quota_fetch_enabled 默认开（每轮现读，与设置页
    //    即时生效惯例一致）；额度失败有独立降级语义（展示最近快照，fetch 内
    //    已留痕），不计入 degraded——degraded 表达"观测能力受损"，而非
    //    "外部接口抖动"
    if !store
        .get_setting("quota_fetch_enabled")
        .map(|v| v != "0")
        .unwrap_or(true)
    {
        return;
    }
    let due_ids: Vec<String> = adapters
        .iter()
        .map(|a| a.account_id().to_string())
        .filter(|id| {
            let (last, streak) = match sched.get(id) {
                Some(s) => (s.last_attempt_ms, s.fail_streak),
                None => (0, 0),
            };
            let interval =
                if streak == 0 { QUOTA_REFRESH_MS } else { quota_backoff_ms(streak) };
            now - last >= interval
        })
        .collect();
    // 启动风暴间隔按"第几个首次尝试实例"计数（原实现用 due 列表位置，
    // 首试件排在首位时不睡、常规件后的首试件才睡，语义不精确——2026-09-29 修正）
    let mut first_seen_idx = 0usize;
    for id in &due_ids {
        let Some(adapter) = adapters.iter().find(|a| a.account_id() == id) else {
            continue;
        };
        let s = sched.entry(id.clone()).or_default();
        let is_first_attempt = s.last_attempt_ms == 0;
        if is_first_attempt && first_seen_idx > 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                QUOTA_STARTUP_STAGGER_MS as u64,
            ));
        }
        if is_first_attempt {
            first_seen_idx += 1;
        }
        s.last_attempt_ms = now;
        match adapter.fetch() {
            Ok(QuotaSnapshot::Windows(rows)) => {
                for r in &rows {
                    store.insert_quota(r);
                }
                s.fail_streak = 0;
            }
            Ok(QuotaSnapshot::Balance(row)) => {
                store.insert_balance(&row);
                s.fail_streak = 0;
            }
            Err(e) => {
                s.fail_streak += 1;
                log::warn!(
                    "[额度] {}（{}）查询失败，{}min 后重试：{e}",
                    adapter.id(),
                    id,
                    quota_backoff_ms(s.fail_streak) / 60_000
                );
            }
        }
    }
}

/// 启动额度调度工作线程。notify 接收额度页「查询」（provider_account_refresh）
/// 成功落库后的实例 id，用于复位退避——发送端由 lib.rs 以 Tauri state 托管
pub fn spawn_quota_worker(store: Arc<Store>, notify: Receiver<String>) {
    std::thread::Builder::new()
        .name("quota-worker".into())
        .spawn(move || {
            // 初始装载（含 GLM 存量凭据一次性迁移与零实例厂商发现，M3-2/M3-4；
            // 自 Aggregator::new 迁来——适配器生命周期从此归本线程独占）
            let mut adapters = bootstrap::load_instances(store.clone());
            let mut sig = provider_sig(&store.list_provider_accounts());
            if adapters.is_empty() {
                log::info!("无启用的供应商实例：额度功能区停用（可在设置页配置实例）");
            } else {
                log::info!(
                    "[额度] 已装载 {} 个供应商实例：{}",
                    adapters.len(),
                    adapters
                        .iter()
                        .map(|a| format!("{}({})", a.id(), a.account_id()))
                        .collect::<Vec<_>>()
                        .join("、")
                );
            }
            let mut sched: HashMap<String, QuotaSched> = HashMap::new();
            // 清理首拍锚定启动时刻（主线程启动清理已执行过一次，24h 后接力）
            let mut last_cleanup_ms = now_ms();
            loop {
                // 单轮 panic 不允许杀死线程（额度静默停摆）：与聚合 tick 的
                // catch_unwind 同款纪律，panic 已由全局钩子落盘
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_once(&store, &mut adapters, &mut sig, &mut sched, &mut last_cleanup_ms, &notify)
                }));
                if result.is_err() {
                    log::error!("[额度] 调度轮 panic（线程续跑，本轮跳过）");
                }
                std::thread::sleep(std::time::Duration::from_millis(WORKER_POLL_MS));
            }
        })
        .expect("额度调度线程启动失败");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 退避阶梯（06-PLAN §5）：连续失败 1/2/3 次按 5/10/20min，≥4 次封顶 60min
    #[test]
    fn test_quota_backoff_ladder() {
        assert_eq!(quota_backoff_ms(1), 5 * 60_000);
        assert_eq!(quota_backoff_ms(2), 10 * 60_000);
        assert_eq!(quota_backoff_ms(3), 20 * 60_000);
        assert_eq!(quota_backoff_ms(4), 60 * 60_000);
        assert_eq!(quota_backoff_ms(99), 60 * 60_000, "封顶 60min");
    }

    /// 实例表指纹（M3-4 热刷新输入）：顺序无关；增删改（updated_at 变化）可感知
    #[test]
    fn test_provider_sig() {
        let mk = |id: &str, updated_at: i64| crate::store::ProviderAccount {
            id: id.into(),
            kind_id: "glm".into(),
            alias: id.into(),
            base_override: None,
            cred_kind: "plain".into(),
            cred_value: None,
            note: None,
            enabled: true,
            in_island: true,
            origin: "manual".into(),
            created_at: 0,
            updated_at,
        };
        let a = vec![mk("a", 1), mk("b", 2)];
        let b = vec![mk("b", 2), mk("a", 1)];
        assert_eq!(provider_sig(&a), provider_sig(&b), "顺序无关");
        let c = vec![mk("a", 1), mk("b", 3)];
        assert_ne!(provider_sig(&a), provider_sig(&c), "updated_at 变化可感知");
    }
}
