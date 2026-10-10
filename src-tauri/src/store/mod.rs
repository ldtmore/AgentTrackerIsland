//! 本地存储层：AgentTrackerIsland 自库（SQLite）的打开/迁移/读写封装。
//! 设计依据 docs/02-DESIGN.md §3；红线③（顺序无关）由幂等键与水位保证。
//! 线程模型：Connection 非 Sync，用 Mutex 包裹，单写多读经同一锁串行（M0 规模足够）。
//! 锁策略：中毒后自恢复（审查 1.1）——单次 panic 不应让后续所有调用连锁失败。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension};

/// 初始化迁移脚本（0001）
const MIGRATION_0001: &str = include_str!("migrations/0001_init.sql");
/// 0002：用量表会话索引（会话级批量聚合加速）+ 移除从未使用的 watermarks.last_offset 列
const MIGRATION_0002: &str = include_str!("migrations/0002_indexes.sql");
/// 0003：幂等键升级 source_id + 后台用量标记 is_background（数据准确性治理 2026-09-21）；
/// 重建空表并在 app_settings 写 usage_rebuild_pending，由聚合器首轮归零水位全量回溯
const MIGRATION_0003: &str = include_str!("migrations/0003_source_key.sql");
/// 0004：无条件重建 usage_records（0003 落地修复）——0003 首版 SQL 的
/// CREATE IF NOT EXISTS 在已有旧表的库上被静默跳过但版本号已消耗，此类库的表
/// 停留在旧结构；0004 对任何中间状态一次收敛（已是新结构的库多重建一次，幂等无害）
const MIGRATION_0004: &str = include_str!("migrations/0004_rebuild_usage.sql");

/// 重建标志键（0003 写入，聚合器消费后删除）
pub const REBUILD_PENDING_KEY: &str = "usage_rebuild_pending";

/// 用户模型单价覆盖（迁移 0005，M3-1）：单价来源优先级 = 本表 → LiteLLM 快照
const MIGRATION_0005: &str = include_str!("migrations/0005_model_price_overrides.sql");
/// 供应商实例表（迁移 0006，M3-2）：用户录入的"某厂商的一个账号"
const MIGRATION_0006: &str = include_str!("migrations/0006_provider_accounts.sql");
/// 快照表扩实例维度 + 余额快照表（迁移 0007，M3-2）
const MIGRATION_0007: &str = include_str!("migrations/0007_quota_account_balance.sql");
/// 常驻查询索引补齐（迁移 0008，2026-09-29 审查修复 #4）：status_events 会话
/// 索引＋快照表 fetched_at 索引＋latest_quotas 窗口函数分区表达式索引
const MIGRATION_0008: &str = include_str!("migrations/0008_indexes.sql");
/// 别名全局唯一索引（迁移 0009，2026-09-30 D14 升级）：跨厂商唯一；建索引前
/// 的存量重名清洗由 migrate() 在同事务内调 dedup_provider_aliases 完成
const MIGRATION_0009: &str = include_str!("migrations/0009_provider_alias_global_unique.sql");
/// 0010：idx_usage_session 升级为 (session_id, ts) 复合索引（消三处 TEMP B-TREE 排序）
const MIGRATION_0010: &str = include_str!("migrations/0010_usage_session_ts_index.sql");
/// 0011：实例表加用户自定义排序列（设置页卡片拖拽排序），存量按创建序回填（升级零感知）
const MIGRATION_0011: &str = include_str!("migrations/0011_provider_sort_order.sql");


// ===== 领域子模块（2026-10-03 审查拆分，纯移动零逻辑变更）=====
mod providers;
mod quota;
mod report;
mod usage;
/// 一条 token 用量流水（来自任一 Agent 适配器的增量采集）
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub session_id: String,
    pub agent: String,
    pub model: String,
    pub provider: Option<String>,
    pub ts: i64, // Unix 毫秒
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub error_type: Option<String>,
    /// 真实消息身份（2026-09-21 双计根治）：CC=message.id(+requestId)、
    /// ZCode=源库行 id、cost 校准行='cost:{sid}:{model}'；同源多行靠它幂等
    pub source_id: Option<String>,
    /// 后台用量校准行（cost-state 差值）：token 计入消耗，调用次数不计
    pub is_background: bool,
}

/// 一条额度快照（来自 Provider 适配器）。provider 存厂商 kind_id；
/// account_id 为实例归属（M3-2 起，0007 前的存量行为 NULL，仅作历史展示）
#[derive(Debug, Clone)]
pub struct QuotaRow {
    pub provider: String,
    pub account_id: Option<String>,
    pub window_kind: String, // '5h' | 'weekly'
    pub used_percent: Option<f64>,
    pub used_tokens: Option<i64>,
    pub reset_at: Option<i64>,
    pub fetched_at: i64,
}

/// 一条货币余额快照（Balance 口径厂商，M3-3 起有生产者）
#[derive(Debug, Clone)]
pub struct BalanceRow {
    pub account_id: String,
    pub currency: String,       // "CNY" | "USD"
    pub total: f64,             // 总余额
    pub granted: Option<f64>,   // 赠金（无则 None，UI 不显示拆分行）
    pub available: Option<f64>, // 可用余额（无则 None）
    pub fetched_at: i64,
}

/// 供应商实例（provider_accounts 行，M3-2）：用户录入的"某厂商的一个账号"，
/// 纯数据结构——UI 与调度全部面向实例（06-PLAN §2.1）
#[derive(Debug, Clone)]
pub struct ProviderAccount {
    pub id: String,
    pub kind_id: String,
    pub alias: String,
    /// 覆盖厂商默认端点（None = 用 ProviderKindDef.default_base）
    pub base_override: Option<String>,
    /// 'plain'（真值在钥匙串）| 'env'（cred_value 存变量名）
    pub cred_kind: String,
    /// env：变量名；plain：None（真值在钥匙串）
    pub cred_value: Option<String>,
    /// 备注（discovered 实例的来源留痕）
    pub note: Option<String>,
    pub enabled: bool,
    pub in_island: bool,
    /// 'manual'（用户录入）| 'discovered'（发现器生成）
    pub origin: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 会话用量四项拆解（2026-09-18 展示改造）：相加即总消耗，与官方账单同口径——
/// 此前只展示 input+output，不含缓存，导致与任何官方后台数字都对不上
#[derive(Debug, Clone, Default)]
pub struct TokenBreakdown {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
}

impl TokenBreakdown {
    /// 总消耗（四项相加，账单口径）
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }
}

/// 存储句柄：克隆 Arc 后全局共享
pub struct Store {
    conn: Mutex<Connection>,
    /// 各分组（厂商，窗口，实例）最新额度快照缓存（2026-10-10 审查新增）：
    /// insert_quota 写库成功即同步维护，latest_quotas 快路径直读，消除聚合 tick
    /// （活跃档 1s）上对 quota_snapshots 的全表窗口扫描；None＝待回源（重启首轮，
    /// 或清理/回填改写历史分组后整体失效）
    quota_latest_cache:
        Mutex<Option<std::collections::HashMap<(String, String, Option<String>), QuotaRow>>>,
}

/// pace 燃烧速度预测结果（2026-09-29 审查新增 #21，对标 QuotaBar/lazyagent）：
/// 按近期采样斜率预测"本次窗口撑不撑得到重置"——把额度监控从被动看静态
/// 百分比升级为主动预警。纯函数＋单测锁定
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PaceResult {
    /// 近期消耗速度（%/小时，负增长按 0 处理）
    pub rate_per_hour: f64,
    /// 重置时刻的预测值（%）；reset_at 未知为 None
    pub projected_at_reset: Option<f64>,
    /// 以当前速度到 100% 的分钟数；速度≈0 为 None
    pub minutes_to_exhaust: Option<i64>,
}

/// 由采样点序列（fetched_at 升序，毫秒）计算 pace：
/// ① 锯齿窗口重置会产生"回落"点，只取最后一次回落之后的段；
/// ② 段内首尾两点求斜率，跨度 <15min 视为噪声不预测；
/// ③ 斜率为负（用量回落，如数据修正）按 0 处理
pub fn compute_pace(pts: &[(i64, f64)], reset_at: Option<i64>) -> Option<PaceResult> {
    if pts.len() < 2 {
        return None;
    }
    // 最后一次显著回落（>1 个百分点）之后才是当前窗口的爬升段
    let start = pts
        .windows(2)
        .rposition(|w| w[1].1 < w[0].1 - 1.0)
        .map(|i| i + 1)
        .unwrap_or(0);
    let seg = &pts[start..];
    if seg.len() < 2 {
        return None;
    }
    let (t1, p1) = seg[0];
    let (t2, p2) = seg[seg.len() - 1];
    let dt_h = (t2 - t1) as f64 / 3_600_000.0;
    if dt_h < 0.25 {
        return None; // 跨度不足 15min：斜率噪声过大，不预测（宁可少说不乱说）
    }
    let rate = ((p2 - p1) / dt_h).max(0.0);
    let projected_at_reset = reset_at.map(|r| {
        let hours_left = ((r - t2) as f64 / 3_600_000.0).max(0.0);
        (p2 + rate * hours_left).clamp(0.0, 999.0)
    });
    let minutes_to_exhaust =
        if rate > 0.5 { Some((((100.0 - p2) / rate) * 60.0) as i64) } else { None };
    Some(PaceResult { rate_per_hour: rate, projected_at_reset, minutes_to_exhaust })
}

impl Store {
    /// 打开（或创建）数据库并执行迁移；父目录不存在时自动创建
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        // 跨进程并发护栏（2026-10-03 审查修复）：双开场景（开机自启＋手点图标）或
        // 外部工具占库时，写库/迁移撞 SQLITE_BUSY 不再瞬时失败——给 5s 等待窗口。
        // 必须在 journal_mode 切换与迁移之前设置（这两步本身也可能撞 BUSY）
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::migrate(&conn)?;
        // SQLite 官方建议周期性执行：优化查询规划器统计（开销极小）
        let _ = conn.execute_batch("PRAGMA optimize;");
        Ok(Self { conn: Mutex::new(conn), quota_latest_cache: Mutex::new(None) })
    }

    /// 迁移：按 user_version 顺序执行。每号迁移在独立事务内完成"SQL 执行＋
    /// 版本号写入"（2026-09-29 审查修复 #6）：原 execute_batch 逐句自动提交，
    /// 0007 若 ALTER 成功而 CREATE INDEX 失败，user_version 停在 6，下次启动
    /// 重跑撞 duplicate column 报错 → Store::open 失败 → 应用拒绝启动；事务化
    /// 后部分失败即整体回滚，版本号不前进，下次完整重试
    fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        /// (SQL, 日志描述, 是否 info 级；建库/小表迁移走 debug 防刷屏)
        const MIGRATIONS: &[(&str, &str, bool)] = &[
            (MIGRATION_0001, "0001 建库", false),
            (MIGRATION_0002, "0002 用量索引", false),
            (MIGRATION_0003, "0003 幂等键升级 source_id（用量表已清空待全量回溯重建）", true),
            (MIGRATION_0004, "0004 无条件重建用量表（修复 0003 可能的静默跳过）", true),
            (MIGRATION_0005, "0005 模型单价覆盖表", false),
            (MIGRATION_0006, "0006 供应商实例表", true),
            (MIGRATION_0007, "0007 快照扩实例维度＋余额快照表", true),
            (MIGRATION_0008, "0008 常驻查询索引补齐（status_events/快照 fetched_at/最新额度分区）", true),
            (MIGRATION_0009, "0009 别名全局唯一索引（跨厂商，先清洗存量重名）", true),
            (MIGRATION_0010, "0010 会话明细索引升级（session_id, ts 复合）", false),
            (MIGRATION_0011, "0011 实例表加用户自定义排序列（拖拽排序配套）", false),
        ];
        let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        // 降级安装检测（2026-10-03 审查补充）：库版本高于本程序已知版本＝新版本
        // 建的库被旧二进制打开（0003「被骗库」的镜像场景）。观测工具不拒绝启动，
        // warn 留痕按现状继续——旧代码只读写自己认识的列，新列被无视是无害降级
        if ver > MIGRATIONS.len() as i64 {
            log::warn!(
                "[存储] 库版本 {ver} 高于本程序支持的 {}（可能被更新版本创建过），按现状继续",
                MIGRATIONS.len()
            );
        }
        for (i, (sql, desc, info)) in MIGRATIONS.iter().enumerate() {
            let target = (i + 1) as i64;
            if ver < target {
                let tx = conn.unchecked_transaction()?;
                // 0009 特例（2026-09-30 D14 升级）：唯一索引对存量重名数据必失败
                // ＝迁移回滚＝每次启动重试每次失败，应用起不来；重名清洗（Rust 侧
                // 追加·N 后缀）与建索引、版本号写入同进同退
                if target == 9 {
                    let renamed = Self::dedup_provider_aliases(&tx)?;
                    if renamed > 0 {
                        log::info!("[存储] 迁移 0009 前清洗跨厂商重名别名 {renamed} 个（自动追加·N 后缀，详见上方逐条留痕）");
                    }
                }
                tx.execute_batch(sql)?;
                tx.pragma_update(None, "user_version", target)?;
                tx.commit()?;
                if *info {
                    log::info!("[存储] 迁移 {desc} 执行完成");
                } else {
                    log::debug!("[存储] 迁移 {desc} 执行完成");
                }
            }
        }
        Ok(())
    }

    /// 0009 前置清洗：跨厂商重名别名追加「·N」后缀（2026-09-30 D14 升级）。
    /// 唯一索引建不起来＝应用起不来，所以只清洗不报错；改名是系统行为，
    /// 不动 updated_at（不伪造"最近更新"）。按创建序保留每组的第一个，
    /// 后续者取最小可用 N（对含存量与本次已改名的全量集合查重，防「GLM·2」
    /// 本身已被占用）。返回改名条数；无重名时一次查询直通
    fn dedup_provider_aliases(conn: &Connection) -> rusqlite::Result<usize> {
        let rows: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, alias FROM provider_accounts ORDER BY created_at ASC, id ASC",
            )?;
            let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            mapped.filter_map(|x| x.ok()).collect()
        };
        let mut renamed = 0usize;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        // 已占用集合含全部现有别名：新后缀名既不撞存量也不撞本次已改出的名
        let mut used: std::collections::HashSet<String> =
            rows.iter().map(|(_, a)| a.clone()).collect();
        for (id, alias) in &rows {
            if !seen.insert(alias.clone()) {
                // 重名者：从 2 起找最小未占用的「别名·N」
                let mut n = 2u32;
                let candidate = loop {
                    let c = format!("{alias}·{n}");
                    if !used.contains(&c) {
                        break c;
                    }
                    n += 1;
                };
                conn.execute(
                    "UPDATE provider_accounts SET alias = ?1 WHERE id = ?2",
                    params![candidate, id],
                )?;
                log::info!(
                    "[存储] 别名 {alias} 跨厂商重名，实例 {id} 已改名为 {candidate}（0009 全局唯一迁移清洗，可在设置页自行重命名）"
                );
                used.insert(candidate);
                renamed += 1;
            }
        }
        Ok(renamed)
    }

    /// 取连接：Mutex 中毒后直接恢复内容继续用（Connection 内容在事务边界始终一致，
    /// 单次 panic 不应放大为全应用连锁失败——审查 1.1）
    fn lock_conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 设置项读写（「upsert 会话元数据」「编辑实例」的 doc 与 allow 属性系
    /// 2026-10-03 域拆分时误留于此，已归还各自函数——usage.rs/providers.rs）
    pub fn get_setting(&self, key: &str) -> Option<String> {
        let conn = self.lock_conn();
        conn.query_row("SELECT value FROM app_settings WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute(
            "INSERT INTO app_settings(key, value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        ) {
            // 留痕（四轮审查）：hook 偏移/岛位置等关键键写入失败若静默，
            // 重启后旧偏移重放会产生重复审计行、位置丢失无线索可查
            log::warn!("[存储] 设置写入失败（key={key}）：{e}");
        }
    }

    // ===== 供应商实例 CRUD（M3-2 最小集：迁移/调度/装载用；编辑/删除 M3-4 随设置页补） =====

    /// 读取全部设置（设置页展示）
    pub fn all_settings(&self) -> std::collections::HashMap<String, String> {
        let conn = self.lock_conn();
        let Ok(mut stmt) = conn.prepare("SELECT key, value FROM app_settings") else {
            return std::collections::HashMap::new();
        };
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] all_settings 查询失败（本轮返回空降级）：{e}");
                std::collections::HashMap::new()
            },
        }
    }

}

/// 单维度分组行（Agent/项目/模型/供应商共用）：token 总量 + 调用次数 + 估算成本
#[derive(Debug, Clone, serde::Serialize)]
pub struct SliceUsage {
    pub label: String,
    pub total: i64,
    pub calls: i64,
    /// 估算成本（美元；单价来源优先级 override→LiteLLM 快照，按当前单价、
    /// 价格变动不追溯——D12。缺价模型该部分计 0，由汇总卡缺价计数提示）
    pub cost: f64,
}

/// 汇总卡指标（当前范围＋筛选下的全量口径）。
/// R2 扩展：duration/ttft 为 Option——转录无时长字段的 Agent（Claude Code）
/// 全 NULL 时 SUM/AVG 得 None，前端显示 —（降级而非误报 0）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SummaryStats {
    pub total_tokens: i64,
    pub calls: i64,
    pub sessions: i64,
    pub errors: i64,
    /// 模型生成时长合计（毫秒）
    pub duration_ms: Option<i64>,
    /// 平均首字延迟 TTFT（毫秒）
    pub ttft_avg_ms: Option<i64>,
    /// 思考 token 合计（思考占比分子）
    pub reasoning_tokens: i64,
    /// 输入+输出合计（思考占比分母；缓存读写不计入，口径见报表页脚注）
    pub billable_tokens: i64,
    /// 输入 token 合计（缓存命中率分母之一，2026-09-24 报表改造）
    pub input_total: i64,
    /// 缓存读 token 合计（缓存命中率分子）
    pub cache_read_total: i64,
    /// 估算成本合计（美元；None=范围内无任何模型可计价；按当前单价、不追溯——D12）
    pub est_cost: Option<f64>,
    /// 缺单价的模型个数（成本列显示「--」；可用单价覆盖表补齐）
    pub missing_price_count: usize,
}

/// 趋势行：时间桶 + 四项用量 + 调用次数 + 生成时长（前端切换指标不再回查）
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrendRow {
    pub bucket: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_creation: i64,
    /// 思考 token 合计（不在账单四项内，tooltip 单列一行展示）
    pub reasoning: i64,
    pub calls: i64,
    /// 该桶生成时长合计（毫秒）；无时长数据为 None
    pub duration_ms: Option<i64>,
    /// 该桶估算成本（美元；按模型单价逐行乘价后归并，缺价模型计 0）
    pub cost: f64,
}

/// 趋势维度切片（2026-09-29 审查修复 #9a）：时间桶×维度值（Agent/模型）的
/// token 合计——趋势图切换"按 Agent／按模型"堆叠时的数据源；只做 token 维度
/// （次数/时长/成本是单柱指标，无需分系列）
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrendSlice {
    pub bucket: String,
    /// 维度值（Agent id 或小写模型名；项目维度空串=未知项目同口径）
    pub label: String,
    /// 四项 token 合计（账单口径含缓存）
    pub total: i64,
}

/// 额度历史采样点（额度消耗曲线用；used_percent 为空的历史行不入列）。
/// account_id 为实例归属（2026-09-29 审查修复 #8：同厂商多实例的采样点
/// 原混进一条线互相穿插跳变，前端按 (provider, account_id, window) 分线）
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuotaPoint {
    pub provider: String,
    pub account_id: Option<String>,
    pub window_kind: String,
    pub fetched_at: i64,
    pub used_percent: f64,
}

/// 额度曲线实例标签（2026-09-29 审查修复 #8 配套）：曲线序列名的解析依据
/// （厂商中文名＋用户别名；含停用实例——历史曲线里的停用实例快照也要能解名）
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuotaAccountLabel {
    pub account_id: String,
    /// 厂商 kind id（与 QuotaPoint.provider 同值）
    pub provider: String,
    /// 厂商显示名（注册表投影；缺厂商兜底 kind id）
    pub kind_name: String,
    /// 用户起的实例别名（可空）
    pub alias: Option<String>,
}

/// 热力图单元：星期×小时，token 与次数双指标（前端切换）
#[derive(Debug, Clone, serde::Serialize)]
pub struct HeatCell {
    pub weekday: i32, // 0=周日 … 6=周六（SQLite strftime %w）
    pub hour: i32,    // 0–23
    pub total: i64,
    pub calls: i64,
}

/// 会话中心行（会话窗口主查询：范围＋筛选下逐会话聚合，状态取自 sessions 表最后已知值）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionRow {
    pub session_id: String,
    pub agent: String,
    /// 最后已知状态（聚合器每 tick 落库；无 sessions 行时为 None，按已结束展示）
    pub state: Option<String>,
    pub model: Option<String>,
    pub project_dir: Option<String>,
    pub title: Option<String>,
    pub first_ts: i64,
    pub last_ts: i64,
    pub calls: i64,
    pub total_tokens: i64,
    /// 四项拆解（悬浮明细用，与账单同口径）
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    /// 思考 token 合计（M1-11 补充露出；报表「思考占比」同源字段）
    pub reasoning_tokens: i64,
    /// 模型生成时长合计（毫秒）；转录无时长字段的 Agent（Claude Code）为 None
    pub duration_ms: Option<i64>,
    /// 平均首字延迟（毫秒）；无 ttft 记录的会话为 None
    pub ttft_avg_ms: Option<i64>,
    /// 出错的调用条数（0 = 从未出错）
    pub errors: i64,
    /// 出现过的错误类型（逗号分隔去重；从未出错为 None）
    pub error_types: Option<String>,
}

/// 会话分页结果（page_size 随行下发，前端页数计算免双源常量）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionPage {
    pub total: i64,
    pub page_size: i64,
    pub rows: Vec<SessionRow>,
}

/// 会话详情：单次模型调用行（抽屉「调用流水」，M1-11）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionCallRow {
    pub ts: i64,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub duration_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub error_type: Option<String>,
}

/// 会话详情：状态事件行（抽屉「状态时间线」，M1-11）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionEventRow {
    pub ts: i64,
    pub hook: Option<String>,
    pub payload: Option<String>,
}

/// 会话详情包（调用流水 + 状态时间线）。
/// calls_total / events_total 为库中真实总数：列表受 LIMIT 截断时，
/// 前端据此诚实展示「N / 共 M」，避免截断后的 N 冒充全量
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SessionDetail {
    pub calls: Vec<SessionCallRow>,
    pub events: Vec<SessionEventRow>,
    pub calls_total: i64,
    pub events_total: i64,
}

/// 筛选下拉选项（只受范围影响、不受其他筛选影响——保证任意组合都能选中）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FilterOptions {
    pub agents: Vec<String>,
    /// 项目目录全路径列表；空串表示"未知项目"（project_dir 缺失的会话）
    pub projects: Vec<String>,
    pub models: Vec<String>,
    /// 供应商列表（与维度条同口径：缺失归 'unknown'）
    pub providers: Vec<String>,
    /// 额度曲线实例标签（2026-09-29 审查修复 #8 配套）：曲线序列名解析依据；
    /// 含停用实例（历史曲线里停用实例的快照也要能解名）
    pub quota_accounts: Vec<QuotaAccountLabel>,
}

/// 整页报表快照（单命令返回，图与图之间口径一致）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReportSnapshot {
    pub summary: SummaryStats,
    /// 上一等长周期汇总（环比基准）：today→昨天、7d/30d/90d→紧邻上一段、all→无
    pub prev_summary: Option<SummaryStats>,
    pub trend: Vec<TrendRow>,
    /// 趋势维度切片（#9a）：趋势图"按 Agent／按模型"堆叠切换的数据源
    pub trend_agent_slices: Vec<TrendSlice>,
    pub trend_model_slices: Vec<TrendSlice>,
    pub by_agent: Vec<SliceUsage>,
    pub by_project: Vec<SliceUsage>,
    pub by_model: Vec<SliceUsage>,
    pub by_provider: Vec<SliceUsage>,
    /// 错误类型分布（仅 error_type 非空的记录；限流/取消/网络等）
    pub by_error: Vec<SliceUsage>,
    /// 按模型平均首字延迟（仅含上报 ttft 的前台调用；无数据为空）
    pub ttft_by_model: Vec<SliceUsage>,
    /// 额度消耗历史采样（与范围档同窗口；未配置额度为空，前端整卡隐藏）
    pub quota_curve: Vec<QuotaPoint>,
    pub heatmap: Vec<HeatCell>,
    pub options: FilterOptions,
}

/// 「已结束」判定阈值：空闲超过该时长按已结束展示。pub 化＋display_constants
/// 命令下发（2026-09-29 审查修复 #20）：原与前端 shared/sessionDisplay.ts 的
/// ENDED_AFTER_MS 两处手工同步有漂移风险，改为后端单真值源、前端挂载时拉取
pub const ENDED_AFTER_MS: i64 = 2 * 3_600_000;

/// 本机今日零点（毫秒）：交给 SQLite 按本机时区计算（'localtime' 读 OS 时区），
/// 与按日聚合口径同源，避免 Rust 侧手写时区/夏令时换算。
/// 末尾 'utc' 必不可少：把"本地零点"字符串按本地时区转回 UTC 再取 epoch
/// （缺了会把本地字面时间当 UTC，偏移一个时区差）。
/// 拆分注（2026-10-03）：usage（today_usage_now）与 report（budget_usage）
/// 两个子模块共用，故留在 mod.rs
fn today_start_ms(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT CAST(strftime('%s','now','localtime','start of day','utc') AS INTEGER)*1000",
        [],
        |r| r.get(0),
    )
    .ok()
}

/// 本月 1 日 0 点（本机时区，#23 月预算口径）
fn month_start_ms(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT CAST(strftime('%s','now','localtime','start of month','utc') AS INTEGER)*1000",
        [],
        |r| r.get(0),
    )
    .ok()
}

/// 当前 Unix 毫秒
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// CSV 字段转义：含逗号/引号/换行时双引号包裹并双写内部引号（RFC 4180）
/// LIKE 通配符转义（\ % _）：用户关键字原样匹配而非当通配符解释，
/// 配合 SQL 里的 ESCAPE '\\' 使用（反斜杠本身先翻转，避免二次歧义）
fn escape_like(kw: &str) -> String {
    kw.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// CSV 字段转义：含逗号/引号/换行时双引号包裹并双写内部引号（RFC 4180）
fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 分页断言引用报表域常量（2026-10-03 拆分后随域迁至 report.rs）
    use super::report::SESSION_PAGE_SIZE;

    /// 生成临时测试库路径（进程级唯一，避免并行测试互踩）
    fn tmp_db(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("at-test-{}-{}.db", tag, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn sample_usage(ts: i64) -> UsageRow {
        UsageRow {
            session_id: "zcode:abc".into(),
            agent: "zcode".into(),
            model: "glm-5.3".into(),
            provider: Some("glm".into()),
            ts,
            input_tokens: Some(1000),
            output_tokens: Some(200),
            reasoning_tokens: Some(50),
            cache_read_tokens: Some(3000),
            cache_creation_tokens: Some(0),
            duration_ms: Some(7812),
            ttft_ms: Some(900),
            error_type: None,
            source_id: Some(format!("test:{ts}")),
            is_background: false,
        }
    }

    #[test]
    fn test_open_and_migrate() {
        let path = tmp_db("migrate");
        {
            let store = Store::open(&path).unwrap();
            store.set_setting("k", "v");
            assert_eq!(store.get_setting("k").as_deref(), Some("v"));
        }
        // 重复打开：迁移幂等，数据仍在
        let store2 = Store::open(&path).unwrap();
        assert_eq!(store2.get_setting("k").as_deref(), Some("v"));
        let _ = std::fs::remove_file(&path);
    }

    /// 0003 首版 SQL 曾在已有旧表的库上被 CREATE IF NOT EXISTS 静默跳过，
    /// 但 user_version 已消耗到 3——此类"被骗库"必须由 0004 无条件重建收敛。
    /// 回归锁定：ver=3 + 旧 13 列结构的库，open 后应升级为可用新结构
    /// （带 source_id/is_background 的行能成功写入）
    #[test]
    fn test_migrate_rescues_deceived_v3_db() {
        let path = tmp_db("deceived");
        {
            // 手工构造被骗库：旧 13 列结构 + user_version=3 + 重建标志已消费。
            // 真实被骗库走过 0001（全量 CREATE IF NOT EXISTS），quota_snapshots 等
            // 基础表必然存在（M3-2 起 0007 要 ALTER 该表，构造缺表会失真），
            // 故按旧结构补齐
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE sessions (
                   id TEXT PRIMARY KEY, agent TEXT NOT NULL, provider TEXT,
                   model TEXT, project_dir TEXT, title TEXT,
                   first_seen_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL,
                   last_usage_at INTEGER NOT NULL
                 );
                 CREATE TABLE status_events (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   agent TEXT, session_id TEXT, hook TEXT, payload TEXT,
                   ts INTEGER NOT NULL
                 );
                 CREATE TABLE watermarks(agent TEXT PRIMARY KEY, last_ts INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE quota_snapshots (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   provider TEXT NOT NULL, window_kind TEXT NOT NULL,
                   used_percent REAL, used_tokens INTEGER, reset_at INTEGER,
                   fetched_at INTEGER NOT NULL
                 );
                 CREATE TABLE usage_records (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id TEXT NOT NULL, agent TEXT NOT NULL, model TEXT NOT NULL,
                   provider TEXT, ts INTEGER NOT NULL,
                   input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER,
                   cache_read_tokens INTEGER, cache_creation_tokens INTEGER,
                   duration_ms INTEGER, ttft_ms INTEGER, error_type TEXT,
                   UNIQUE(agent, session_id, ts, model)
                 );
                 PRAGMA user_version = 3;
                 INSERT INTO app_settings(key, value) VALUES('usage_rebuild_pending', '0');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        // 0004 应已重建为新结构：带新列的行可写入，重建标志重新置位
        let mut row = sample_usage(1_000);
        row.source_id = Some("probe".into());
        assert_eq!(store.insert_usage(&[row]).unwrap(), 1, "被骗库经 0004 后应支持新结构写入");
        assert_eq!(
            store.get_setting(crate::store::REBUILD_PENDING_KEY).as_deref(),
            Some("1"),
            "重建标志应重新置位以触发全量回溯"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_write_failure_contract() {
        // 四轮审查回归锁：写库失败路径的返回契约（红线③防线）
        // ① upsert_session 失败须返回 Err——service 据此跳过签名缓存推进，
        //    否则该会话行持续缺写直到字段变化（库/内存口径分裂）
        let path = tmp_db("write-fail");
        let store = Store::open(&path).unwrap();
        {
            let conn = store.lock_conn();
            conn.execute_batch("DROP TABLE sessions").unwrap();
        }
        assert!(
            store
                .upsert_session("s1", "zcode", None, None, None, None, 1, "idle", None)
                .is_err()
        );
        let _ = std::fs::remove_file(&path);

        // ② insert_usage 单行写失败返回 Ok(0)（确定性行失败不阻断——若因它
        //    返回 Err 会永久卡住水位让整条采集管线停摆，两害取轻）；
        //    事务级失败（开启/提交）才返回 Err，由 service 跳过水位推进兜底
        let path2 = tmp_db("write-fail2");
        let store2 = Store::open(&path2).unwrap();
        {
            let conn = store2.lock_conn();
            conn.execute_batch(
                "CREATE TRIGGER abort_usage BEFORE INSERT ON usage_records
                 BEGIN SELECT RAISE(ABORT, 'injected'); END",
            )
            .unwrap();
        }
        assert_eq!(
            store2.insert_usage(&[sample_usage(1_000)]).unwrap(),
            0,
            "单行失败跳过、空事务提交成功，应返回 Ok(0)"
        );
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn test_usage_idempotent() {
        let path = tmp_db("idem");
        let store = Store::open(&path).unwrap();
        let rows = vec![sample_usage(1_000), sample_usage(2_000)];
        assert_eq!(store.insert_usage(&rows).unwrap(), 2);
        // 同一批再插：全部命中幂等键，0 行新增
        assert_eq!(store.insert_usage(&rows).unwrap(), 0);
        // 交错重复：仅新行入库
        let mut again = rows.clone();
        again.push(sample_usage(3_000));
        assert_eq!(store.insert_usage(&again).unwrap(), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// 最近调用时间：逐会话取 MAX(ts)；无行会话不出现（service 层得 None，
    /// 不冒充活动）；后台行也是真实调用，计入 MAX（CC 假 working 治理配套）
    #[test]
    fn test_latest_call_ts() {
        let path = tmp_db("latest-call-ts");
        let store = Store::open(&path).unwrap();
        let mut a1 = sample_usage(1_000);
        a1.session_id = "claude-code:s1".into();
        let mut a2 = sample_usage(5_000);
        a2.session_id = "claude-code:s1".into();
        let mut a_bg = sample_usage(9_000);
        a_bg.session_id = "claude-code:s1".into();
        a_bg.is_background = true; // cost-state 后台差值行：MAX 应包含
        let mut b1 = sample_usage(2_000);
        b1.session_id = "claude-code:s2".into();
        store.insert_usage(&[a1, a2, a_bg, b1]).unwrap();
        let ids = vec![
            "claude-code:s1".to_string(),
            "claude-code:s2".to_string(),
            "claude-code:s3".to_string(), // 无任何调用行
        ];
        let m = store.latest_call_ts(&ids);
        assert_eq!(m.get("claude-code:s1"), Some(&9_000), "MAX 含后台行");
        assert_eq!(m.get("claude-code:s2"), Some(&2_000));
        assert!(!m.contains_key("claude-code:s3"), "无行会话不出现");
        // 空集安全
        assert!(store.latest_call_ts(&[]).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// pace 纯函数（#21）：重置回落取段／跨度不足不预测／斜率与外推
    #[test]
    fn test_compute_pace() {
        let h = 3_600_000i64;
        // 2 小时 30→50：斜率 10%/h；重置在 3h 后 → 预测 80%；到限 5h
        let pts = vec![(0, 30.0), (2 * h, 50.0)];
        let p = compute_pace(&pts, Some(5 * h)).unwrap();
        assert!((p.rate_per_hour - 10.0).abs() < 1e-9);
        assert!((p.projected_at_reset.unwrap() - 80.0).abs() < 1e-9);
        assert_eq!(p.minutes_to_exhaust, Some(300));
        // 中途回落（窗口重置）：只取回落后的段——(20→30)/1h = 10%/h
        let pts2 = vec![(0, 30.0), (2 * h, 50.0), (3 * h, 60.0), (3 * h + 1, 20.0), (4 * h + 1, 30.0)];
        let p2 = compute_pace(&pts2, None).unwrap();
        assert!((p2.rate_per_hour - 10.0).abs() < 1e-9);
        assert_eq!(p2.projected_at_reset, None, "reset 未知不外推");
        // 跨度不足 15min：斜率噪声过大，不预测
        assert!(compute_pace(&[(0, 10.0), (10 * 60_000, 20.0)], None).is_none());
        // 单点／空序列：不预测
        assert!(compute_pace(&[(0, 10.0)], None).is_none());
        assert!(compute_pace(&[], None).is_none());
        // 静止用量：速度 0，无到限时间
        let p3 = compute_pace(&[(0, 40.0), (2 * h, 40.0)], Some(4 * h)).unwrap();
        assert_eq!(p3.rate_per_hour, 0.0);
        assert_eq!(p3.minutes_to_exhaust, None);
        assert!((p3.projected_at_reset.unwrap() - 40.0).abs() < 1e-9);
    }

    /// claude_usage_rows 必须命中采集层写入的 agent 字面量（'claude-code'，
    /// 见 collector/claude_code.rs id()）——SQL 曾误写 'claude' 致 M3-12
    /// 本地推算数据源恒空、岛/额度页恒显 0%（2026-09-29 审查修复的回归锁）
    #[test]
    fn test_claude_usage_rows_matches_collector_agent_id() {
        let path = tmp_db("claude-usage-rows");
        let store = Store::open(&path).unwrap();
        // 按 sample_usage 四项（1000+200+0+3000=4200）改造出两行 CC＋一行他家
        let mut cc1 = sample_usage(1_000);
        cc1.agent = "claude-code".into();
        cc1.session_id = "claude-code:s1".into();
        let mut cc2 = sample_usage(2_000);
        cc2.agent = "claude-code".into();
        cc2.session_id = "claude-code:s1".into();
        cc2.source_id = Some("test:2000".into());
        let mut other = sample_usage(3_000);
        other.agent = "codex".into();
        other.session_id = "codex:s9".into();
        other.source_id = Some("test:3000".into());
        store.insert_usage(&[cc1, cc2, other]).unwrap();
        let rows = store.claude_usage_rows(0).unwrap();
        assert_eq!(rows.len(), 2, "只取 claude-code 行，他家 Agent 不混入");
        assert_eq!(rows[0], (1_000, 4_200), "四项合计＝input+output+cache_creation+cache_read");
        assert_eq!(rows[1], (2_000, 4_200));
        // since 过滤与升序
        let rows2 = store.claude_usage_rows(1_500).unwrap();
        assert_eq!(rows2.len(), 1);
        assert_eq!(rows2[0].0, 2_000);
        let _ = std::fs::remove_file(&path);
    }

    /// 同幂等键多快照：仅四项合计更大的行才覆盖（与 CC 流式去重口径一致）；
    /// 更小快照重复采集不回退
    #[test]
    fn test_usage_upsert_keeps_max_snapshot() {
        let path = tmp_db("upsert");
        let store = Store::open(&path).unwrap();
        // 首插：流式中途的小快照
        assert_eq!(store.insert_usage(&[sample_usage(1_000)]).unwrap(), 1);
        // 同键更大快照（流式写全）：覆盖
        let mut bigger = sample_usage(1_000);
        bigger.input_tokens = Some(5_000);
        assert_eq!(store.insert_usage(&[bigger]).unwrap(), 1);
        // 库中为覆盖后的值（session_usage_total = input+output）
        assert_eq!(store.session_usage_total("zcode:abc"), 5_200);
        // 更小快照重复采到：不覆盖、不变更
        assert_eq!(store.insert_usage(&[sample_usage(1_000)]).unwrap(), 0);
        assert_eq!(store.session_usage_total("zcode:abc"), 5_200);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_watermark_monotonic() {
        let path = tmp_db("wm");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_watermark("zcode"), 0);
        store.set_watermark("zcode", 500);
        store.set_watermark("zcode", 300); // 回退值不生效
        assert_eq!(store.get_watermark("zcode"), 500);
        let _ = std::fs::remove_file(&path);
    }

    /// 2026-09-21 双计根治回归：
    /// ① 同 source_id（同消息）timestamp 各异的流式复制行 → 合并为一行、保留最大快照
    ///   （旧幂等键 (sid,ts,model) 不含消息身份，跨 tick 分裂时双计 +27.8%）
    /// ② 后台校准行无条件覆盖（差值随 assistant 增长缩小，须跟随重算值而非保留最大）
    /// ③ 调用次数口径：today_usage / 会话页 calls 排除后台行，token 计入
    #[test]
    fn test_source_key_merges_and_background_calls() {
        let path = tmp_db("srckey");
        let store = Store::open(&path).unwrap();
        // 同消息（source_id 相同）两行快照，timestamp 不同（流式复制行实测形态）
        let mut small = sample_usage(1_000);
        small.source_id = Some("msg_a".into());
        small.input_tokens = Some(40);
        let mut big = sample_usage(5_000);
        big.source_id = Some("msg_a".into());
        big.input_tokens = Some(100);
        assert_eq!(store.insert_usage(&[small, big.clone()]).unwrap(), 2);
        // 同批次重复重采（水位余量回退场景）：不再新增
        assert_eq!(store.insert_usage(&[big.clone()]).unwrap(), 0);
        // 库中该消息只有一行（旧键实现下会是两行）
        let n: i64 = {
            let conn = store.lock_conn();
            // 测试内直达连接仅此一处用途：精确断言行数
            conn.query_row(
                "SELECT COUNT(*) FROM usage_records WHERE source_id = 'msg_a'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(n, 1, "同 source_id 必须合并为一行");

        // 后台校准行：差值行先大后小，两次都覆盖（跟随重算值）；
        // 字段形态与 service 层构造一致：只有 input_tokens 承载差值，其余为 None
        let mut bg1 = sample_usage(6_000);
        bg1.source_id = Some("cost:zcode:abc:glm-5.3".into());
        bg1.is_background = true;
        bg1.input_tokens = Some(500);
        bg1.output_tokens = None;
        bg1.reasoning_tokens = None;
        bg1.cache_read_tokens = None;
        bg1.cache_creation_tokens = None;
        bg1.duration_ms = None;
        bg1.ttft_ms = None;
        assert_eq!(store.insert_usage(&[bg1]).unwrap(), 1);
        let mut bg2 = sample_usage(7_000);
        bg2.source_id = Some("cost:zcode:abc:glm-5.3".into());
        bg2.is_background = true;
        bg2.input_tokens = Some(300); // 差值缩小：普通行的"保留最大"不适用
        bg2.output_tokens = None;
        bg2.reasoning_tokens = None;
        bg2.cache_read_tokens = None;
        bg2.cache_creation_tokens = None;
        bg2.duration_ms = None;
        bg2.ttft_ms = None;
        assert_eq!(store.insert_usage(&[bg2]).unwrap(), 1);
        let bg_val: i64 = {
            let conn = store.lock_conn();
            conn.query_row(
                "SELECT input_tokens FROM usage_records WHERE source_id = 'cost:zcode:abc:glm-5.3'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(bg_val, 300, "后台行必须无条件覆盖为最新差值");

        // 调用次数排除后台行；token 计入（100+200+3000+0 普通 + 300 后台）
        let (today_tokens, today_calls) = store.today_usage(0);
        assert_eq!(today_calls, 1, "调用次数不含后台校准行");
        assert_eq!(today_tokens, 3_300 + 300, "token 总量含后台校准行");
        // 会话页 calls 同口径（后台行计入 token 但不计次数）
        let page = store
            .session_page("all", None, None, None, "all", "", "recent", 20, 0)
            .unwrap();
        let row = page.rows.iter().find(|r| r.session_id == "zcode:abc").unwrap();
        assert_eq!(row.calls, 1);
        assert_eq!(row.total_tokens, 3_600);
        let _ = std::fs::remove_file(&path);
    }    #[test]
    fn test_cleanup() {
        let path = tmp_db("clean");
        let store = Store::open(&path).unwrap();
        store.insert_usage(&[sample_usage(1_000), sample_usage(9_000)]).unwrap();        store.insert_quota(&QuotaRow {
            provider: "glm".into(),
            account_id: None,
            window_kind: "5h".into(),
            used_percent: Some(35.0),
            used_tokens: None,
            reset_at: None,
            fetched_at: 800,
        });
        // sessions 行随保留周期清理（2026-09-28）：老行删、活跃行留
        store.upsert_session("t:old", "claude-code", None, None, None, Some("老会话"), 1_000, "offline", None).unwrap();        store.upsert_session("t:new", "claude-code", None, None, None, Some("活跃会话"), 9_000, "idle", None).unwrap();        let removed = store.cleanup_older_than(5_000);
        assert_eq!(removed, 3); // 1 条 usage + 1 条快照 + 1 条老会话行
        // 活跃会话行保留（session_titles 只回报存在的行）
        let titles = store.session_titles(&["t:old".into(), "t:new".into()]);
        assert!(titles.contains_key("t:new"), "活跃会话行应保留");
        assert!(!titles.contains_key("t:old"), "过期会话行应被清理");
        let _ = std::fs::remove_file(&path);
    }

    /// 活跃会话计数（2026-10-03 幽灵列修复回归锁）：近 2 小时口径。原 SQL
    /// 引用从未落库的 last_usage_at 列——报错被 unwrap_or(0) 吞掉恒返 0
    #[test]
    fn test_recent_session_count() {
        let path = tmp_db("rscount");
        let store = Store::open(&path).unwrap();
        let now = now_ms();
        store.upsert_session("t:hot", "claude-code", None, None, None, Some("活跃"), now - 60_000, "idle", None).unwrap();        store.upsert_session("t:cold", "claude-code", None, None, None, Some("陈旧"), now - 3 * 3_600_000, "offline", None).unwrap();        assert_eq!(
            store.recent_session_count(),
            1,
            "近 2 小时内有活动的会话应恰为 1（旧实现因幽灵列恒 0）"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 供应商实例层（M3-2）：实例 CRUD、别名全局唯一（0009）、快照按实例分组、
    /// 存量 NULL 行独立成组、余额快照写入与读取
    #[test]
    fn test_provider_accounts_and_quota_groups() {
        let path = tmp_db("prov");
        let store = Store::open(&path).unwrap();

        // 实例插入与读取（字段全量往返）
        let mk = |id: &str, kind: &str, alias: &str, cred_kind: &str| ProviderAccount {
            id: id.into(),
            kind_id: kind.into(),
            alias: alias.into(),
            base_override: None,
            cred_kind: cred_kind.into(),
            cred_value: (cred_kind == "env").then(|| "MY_KEY_VAR".to_string()),
            note: Some("测试备注".into()),
            enabled: true,
            in_island: true,
            origin: "manual".into(),
            created_at: 1_000,
            updated_at: 1_000,
        };
        store.insert_provider_account(&mk("acc-a", "glm", "GLM", "plain")).unwrap();
        store.insert_provider_account(&mk("acc-b", "glm", "GLM 工作号", "env")).unwrap();
        assert_eq!(store.list_provider_accounts().len(), 2);
        // 同厂商别名唯一（D14）：冲突必须报错
        assert!(store.insert_provider_account(&mk("acc-c", "glm", "GLM", "plain")).is_err());
        // 启停
        store.set_provider_account_enabled("acc-b", false).unwrap();
        let acc_b = store
            .list_provider_accounts()
            .into_iter()
            .find(|a| a.id == "acc-b")
            .unwrap();
        assert!(!acc_b.enabled, "停用后 enabled 应为 false");
        assert_eq!(acc_b.cred_value.as_deref(), Some("MY_KEY_VAR"));

        // 存量行（account_id NULL）与两个实例的快照交错写入
        let mut old = sample_quota(None, "5h", 10.0, 100);
        old.provider = "glm".into();
        store.insert_quota(&old);
        store.insert_quota(&sample_quota(Some("acc-a"), "5h", 50.0, 200));
        store.insert_quota(&sample_quota(Some("acc-a"), "weekly", 60.0, 201));
        store.insert_quota(&sample_quota(Some("acc-b"), "5h", 70.0, 202));
        // 同实例同窗口重复写入：取最新一条
        store.insert_quota(&sample_quota(Some("acc-a"), "5h", 55.0, 300));

        let latest = store.latest_quotas();
        assert_eq!(latest.len(), 4, "NULL 存量组 + acc-a 5h/weekly + acc-b 5h");
        let get = |account: Option<&str>, window: &str| {
            latest
                .iter()
                .find(|q| q.account_id.as_deref() == account && q.window_kind == window)
                .unwrap_or_else(|| panic!("缺少 {}@{window} 的最新快照", account.unwrap_or("NULL")))
        };
        assert_eq!(get(Some("acc-a"), "5h").used_percent, Some(55.0), "同实例同窗口应取最新");
        assert_eq!(get(Some("acc-a"), "weekly").used_percent, Some(60.0));
        assert_eq!(get(Some("acc-b"), "5h").used_percent, Some(70.0));
        assert_eq!(get(None, "5h").used_percent, Some(10.0), "存量 NULL 行应独立成组不丢失");

        // 存量回填：glm 的 NULL 行划归 acc-a 后，NULL 组消失
        assert_eq!(store.backfill_quota_account("glm", "acc-a"), 1);
        let latest = store.latest_quotas();
        assert_eq!(latest.len(), 3, "回填后 NULL 组并入 acc-a");
        assert_eq!(
            get2(&latest, Some("acc-a"), "5h").used_percent,
            Some(55.0),
            "回填不改写各实例最新值"
        );

        // 余额快照：插入两条取最新
        store.insert_balance(&BalanceRow {
            account_id: "acc-a".into(),
            currency: "CNY".into(),
            total: 100.0,
            granted: Some(20.0),
            available: Some(80.0),
            fetched_at: 500,
        });
        store.insert_balance(&BalanceRow {
            account_id: "acc-a".into(),
            currency: "CNY".into(),
            total: 90.5,
            granted: None,
            available: None,
            fetched_at: 600,
        });
        let bal = store.latest_balance("acc-a").unwrap();
        assert_eq!(bal.total, 90.5, "应取最新一条余额");
        assert_eq!(bal.granted, None);
        assert!(store.latest_balance("acc-b").is_none(), "无余额数据返回 None");
        let _ = std::fs::remove_file(&path);
    }

    /// 实例编辑/删除/岛展示集（M3-4 设置页命令的存储层）
    #[test]
    fn test_provider_account_update_delete_in_island() {
        let path = tmp_db("provupd");
        let store = Store::open(&path).unwrap();
        let mk = |id: &str, alias: &str| ProviderAccount {
            id: id.into(),
            kind_id: "glm".into(),
            alias: alias.into(),
            base_override: None,
            cred_kind: "plain".into(),
            cred_value: None,
            note: None,
            enabled: true,
            in_island: true,
            origin: "manual".into(),
            created_at: 1_000,
            updated_at: 1_000,
        };
        store.insert_provider_account(&mk("acc-a", "GLM")).unwrap();
        store.insert_provider_account(&mk("acc-b", "GLM 工作号")).unwrap();

        // 编辑：字段全量更新，updated_at 刷新（热刷新指纹依赖它）
        store
            .update_provider_account(
                "acc-a", "GLM 个人号", Some("https://api.z.ai".into()), "env",
                Some("MY_VAR".into()), None,
            )
            .unwrap();
        let a = store
            .list_provider_accounts()
            .into_iter()
            .find(|x| x.id == "acc-a")
            .unwrap();
        assert_eq!(a.alias, "GLM 个人号");
        assert_eq!(a.base_override.as_deref(), Some("https://api.z.ai"));
        assert_eq!(a.cred_kind, "env");
        assert_eq!(a.cred_value.as_deref(), Some("MY_VAR"));
        assert!(a.updated_at > 1_000, "编辑应刷新 updated_at");

        // 编辑撞同厂商别名唯一：透传 Err
        assert!(
            store
                .update_provider_account("acc-a", "GLM 工作号", None, "plain", None, None)
                .is_err(),
            "同厂商别名冲突应报错"
        );
        // 编辑不存在的实例
        assert!(store
            .update_provider_account("nope", "x", None, "plain", None, None)
            .is_err());

        // 岛展示集切换
        store.set_provider_account_in_island("acc-a", false).unwrap();
        let a = store
            .list_provider_accounts()
            .into_iter()
            .find(|x| x.id == "acc-a")
            .unwrap();
        assert!(!a.in_island);

        // 删除：行消失；不存在时报错；另一实例不受影响
        store.delete_provider_account("acc-a").unwrap();
        assert!(store.delete_provider_account("acc-a").is_err(), "重复删除应报错");
        let rest = store.list_provider_accounts();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, "acc-b");
        let _ = std::fs::remove_file(&path);
    }

    /// 0009 别名全局唯一（2026-09-30 D14 升级）：跨厂商同名插入/改名都必须被
    /// idx_provider_accounts_alias 拦截（同厂商路径由表级 UNIQUE 继续兜底）
    #[test]
    fn test_provider_alias_global_unique() {
        let path = tmp_db("prov-global-alias");
        let store = Store::open(&path).unwrap();
        let mk = |id: &str, kind: &str, alias: &str| ProviderAccount {
            id: id.into(),
            kind_id: kind.into(),
            alias: alias.into(),
            base_override: None,
            cred_kind: "plain".into(),
            cred_value: None,
            note: None,
            enabled: true,
            in_island: true,
            origin: "manual".into(),
            created_at: 1_000,
            updated_at: 1_000,
        };
        store.insert_provider_account(&mk("acc-a", "glm", "GLM")).unwrap();
        // 跨厂商同名插入：0009 唯一索引必须拦截（原口径只拦同厂商，此为升级回归锁）
        assert!(
            store.insert_provider_account(&mk("acc-b", "custom-openai", "GLM")).is_err(),
            "跨厂商同名插入应报错（全局唯一）"
        );
        // 编辑改名撞跨厂商同名：同样拦截
        store.insert_provider_account(&mk("acc-c", "kimi", "Kimi")).unwrap();
        assert!(
            store
                .update_provider_account("acc-a", "Kimi", None, "plain", None, None)
                .is_err(),
            "改名撞跨厂商同名应报错（全局唯一）"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 0009 存量重名清洗：ver=8 旧库含跨厂商重名时，迁移应先改名为「·N」
    /// 再建索引（否则唯一索引失败＝迁移回滚＝应用起不来）；「·N」被占用时
    /// 顺延取最小可用值，本次刚改出的名也要避开
    #[test]
    fn test_migrate_0009_dedups_duplicate_aliases() {
        let path = tmp_db("prov-dedup");
        {
            // 手工构造 ver=8 旧库：需 0006 表结构＋usage_records（0010 起迁移会在
            // 其上重建复合索引；0009 只触碰 provider_accounts，其余迁移按
            // user_version=8 跳过，同 test_migrate_rescues_deceived_v3_db 手法）
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE provider_accounts (
                   id            TEXT PRIMARY KEY,
                   kind_id       TEXT NOT NULL,
                   alias         TEXT NOT NULL,
                   base_override TEXT,
                   cred_kind     TEXT NOT NULL DEFAULT 'plain',
                   cred_value    TEXT,
                   note          TEXT,
                   enabled       INTEGER NOT NULL DEFAULT 1,
                   in_island     INTEGER NOT NULL DEFAULT 1,
                   origin        TEXT NOT NULL DEFAULT 'manual',
                   created_at    INTEGER NOT NULL,
                   updated_at    INTEGER NOT NULL,
                   UNIQUE(kind_id, alias)
                 );
                 CREATE TABLE usage_records (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id TEXT NOT NULL,
                   agent TEXT NOT NULL,
                   model TEXT NOT NULL,
                   ts INTEGER NOT NULL
                 );
                 CREATE INDEX idx_usage_session ON usage_records(session_id);
                 PRAGMA user_version = 8;
                 INSERT INTO provider_accounts(id, kind_id, alias, created_at, updated_at) VALUES
                   ('acc-a', 'glm',           'GLM',   1000, 1000),
                   ('acc-b', 'custom-openai', 'GLM',   2000, 2000),
                   ('acc-c', 'kimi',          'GLM·2', 3000, 3000),
                   ('acc-d', 'deepseek',      'GLM',   4000, 4000);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let alias_of = |id: &str| {
            store
                .list_provider_accounts()
                .into_iter()
                .find(|a| a.id == id)
                .unwrap()
                .alias
        };
        // 创建序首个保留原名；后续者顺延（GLM·2 已被 acc-c 占用→GLM·3；
        // GLM·3 被本次改名占用→acc-d 顺延到 GLM·4）
        assert_eq!(alias_of("acc-a"), "GLM", "每组首个拥有者保留原名");
        assert_eq!(alias_of("acc-b"), "GLM·3", "GLM·2 已被占用应顺延到 GLM·3");
        assert_eq!(alias_of("acc-c"), "GLM·2", "无重名者不应被改动");
        assert_eq!(alias_of("acc-d"), "GLM·4", "应避开本次已改出的 GLM·3");
        // 清洗后索引就位：跨厂商同名插入被拦截（迁移完整性回归锁）
        let mk = |id: &str, kind: &str, alias: &str| ProviderAccount {
            id: id.into(),
            kind_id: kind.into(),
            alias: alias.into(),
            base_override: None,
            cred_kind: "plain".into(),
            cred_value: None,
            note: None,
            enabled: true,
            in_island: true,
            origin: "manual".into(),
            created_at: 5_000,
            updated_at: 5_000,
        };
        assert!(
            store.insert_provider_account(&mk("acc-e", "glm", "GLM·4")).is_err(),
            "清洗建索引后跨厂商同名插入应报错"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 实例拖拽排序（0011 配套）：新实例恒排尾、reorder 全量重写、未知 id 容错忽略
    #[test]
    fn test_provider_account_reorder() {
        let path = tmp_db("prov-reorder");
        let store = Store::open(&path).unwrap();
        let mk = |id: &str, created: i64| ProviderAccount {
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
            created_at: created,
            updated_at: created,
        };
        store.insert_provider_account(&mk("acc-a", 1_000)).unwrap();
        store.insert_provider_account(&mk("acc-b", 2_000)).unwrap();
        store.insert_provider_account(&mk("acc-c", 3_000)).unwrap();
        let ids =
            |s: &Store| s.list_provider_accounts().into_iter().map(|a| a.id).collect::<Vec<_>>();
        assert_eq!(ids(&store), vec!["acc-a", "acc-b", "acc-c"], "初始序＝插入序（新实例排尾）");
        // 拖拽：acc-c 提到最前；未知 id（并发已删）更新 0 行、忽略不报错
        store
            .reorder_provider_accounts(&[
                "acc-c".into(),
                "ghost".into(),
                "acc-a".into(),
                "acc-b".into(),
            ])
            .unwrap();
        assert_eq!(ids(&store), vec!["acc-c", "acc-a", "acc-b"], "重排后按用户序输出");
        // 重排后再新增：MAX+1 基于重排后的值，仍排尾
        store.insert_provider_account(&mk("acc-d", 4_000)).unwrap();
        assert_eq!(ids(&store), vec!["acc-c", "acc-a", "acc-b", "acc-d"]);
        let _ = std::fs::remove_file(&path);
    }

    /// 0011 迁移回填回归锁：ver=10 旧库（无 sort_order 列）升级后，
    /// 初始顺序＝原创建序——老用户升级零感知
    #[test]
    fn test_migrate_0011_backfills_created_order() {
        let path = tmp_db("prov-0011");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE provider_accounts (
                   id            TEXT PRIMARY KEY,
                   kind_id       TEXT NOT NULL,
                   alias         TEXT NOT NULL,
                   base_override TEXT,
                   cred_kind     TEXT NOT NULL DEFAULT 'plain',
                   cred_value    TEXT,
                   note          TEXT,
                   enabled       INTEGER NOT NULL DEFAULT 1,
                   in_island     INTEGER NOT NULL DEFAULT 1,
                   origin        TEXT NOT NULL DEFAULT 'manual',
                   created_at    INTEGER NOT NULL,
                   updated_at    INTEGER NOT NULL,
                   UNIQUE(kind_id, alias)
                 );
                 PRAGMA user_version = 10;
                 INSERT INTO provider_accounts(id, kind_id, alias, created_at, updated_at) VALUES
                   ('acc-b', 'glm', 'B', 2000, 2000),
                   ('acc-a', 'glm', 'A', 1000, 1000);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let ids: Vec<String> =
            store.list_provider_accounts().into_iter().map(|a| a.id).collect();
        assert_eq!(ids, vec!["acc-a", "acc-b"], "迁移回填＝创建序，升级零感知");
        let _ = std::fs::remove_file(&path);
    }

    /// 测试辅助：构造一条额度快照
    fn sample_quota(account: Option<&str>, window: &str, pct: f64, at: i64) -> QuotaRow {
        QuotaRow {
            provider: "glm".into(),
            account_id: account.map(str::to_string),
            window_kind: window.into(),
            used_percent: Some(pct),
            used_tokens: None,
            reset_at: None,
            fetched_at: at,
        }
    }

    /// get 闭包的独立版本（回填断言处重新借用 latest）
    fn get2<'a>(latest: &'a [QuotaRow], account: Option<&str>, window: &str) -> &'a QuotaRow {
        latest
            .iter()
            .find(|q| q.account_id.as_deref() == account && q.window_kind == window)
            .unwrap()
    }

    /// 报表快照/分页（M1-R1）：守恒、维度分组、维度过滤、今日档、非法档拒绝。
    /// 时间用 now 附近样本（today 档必然覆盖；样本时间早于今日零点用 now-3 秒，
    /// 测试进程毫秒级完成，不会跨过午夜边界）
    #[test]
    fn test_report_snapshot() {
        let path = tmp_db("report2");
        let store = Store::open(&path).unwrap();
        let row_sum = 4_200i64; // 样本四项之和（1000+200+3000+0）
        let now = now_ms();

        // 样本工厂：两 Agent × 两项目 × 两模型，zcode 带时长、CC 无时长、1 条限流错误
        let mk = |agent: &str, sid: &str, model: &str, proj: &str, ts: i64, dur: Option<i64>, err: Option<&str>| {
            let full_sid = format!("{agent}:{sid}");
            store.upsert_session(&full_sid, agent, Some("glm"), Some(model), Some(proj), Some("标题"), ts, "idle", None).unwrap();            UsageRow {
                session_id: full_sid,
                agent: agent.into(),
                model: model.into(),
                provider: Some("glm".into()),
                ts,
                input_tokens: Some(1000),
                output_tokens: Some(200),
                reasoning_tokens: Some(50),
                cache_read_tokens: Some(3000),
                cache_creation_tokens: Some(0),
                duration_ms: dur,
                ttft_ms: Some(900),
                error_type: err.map(String::from),
                source_id: Some(format!("{agent}:{sid}:{ts}")),
                is_background: false,
            }
        };
        let rows = vec![
            mk("zcode", "s1", "GLM-5.3", r"F:\projA", now - 1_000, Some(5_000), None),
            mk("zcode", "s2", "glm-5.3-flash", r"F:\projB", now - 2_000, None, Some("rate_limited")),
            mk("claude-code", "s1", "claude-sonnet", r"F:\projA", now - 3_000, None, None),
        ];
        store.insert_usage(&rows).unwrap();
        // 全量快照（all 档，按月分桶）
        let snap = store.report_snapshot("all", None, None, None, None).unwrap();
        assert_eq!(snap.summary.total_tokens, row_sum * 3);
        assert_eq!(snap.summary.calls, 3);
        assert_eq!(snap.summary.sessions, 3);
        assert_eq!(snap.summary.errors, 1);
        // 缓存命中率原料：输入与缓存读合计（样本固定 input=1000、cache_read=3000）
        assert_eq!(snap.summary.input_total, 1000 * 3);
        assert_eq!(snap.summary.cache_read_total, 3000 * 3);

        // Agent 维度：zcode 2 行在前（降序）；模型名大小写归一无影响（此处 zcode 两条模型不同）
        assert_eq!(snap.by_agent.len(), 2);
        assert_eq!(snap.by_agent[0].label, "zcode");
        assert_eq!(snap.by_agent[0].total, row_sum * 2);
        assert_eq!(snap.by_agent[0].calls, 2);

        // 项目维度：projA 2 行在前
        assert_eq!(snap.by_project.len(), 2);
        assert_eq!(snap.by_project[0].label, r"F:\projA");
        assert_eq!(snap.by_project[0].total, row_sum * 2);

        // 模型维度：三条各一组（glm-5.3 大写样本应归一为小写标签）
        assert_eq!(snap.by_model.len(), 3);
        assert!(snap.by_model.iter().all(|m| m.label == m.label.to_lowercase()));

        // 供应商维度（M3-1 模型名归因）：glm 两行一组、claude-sonnet 归 anthropic
        assert_eq!(snap.by_provider.len(), 2);
        assert_eq!(snap.by_provider[0].label, "glm");
        assert_eq!(snap.by_provider[0].total, row_sum * 2);
        assert_eq!(snap.by_provider[1].label, "anthropic");
        assert_eq!(snap.by_provider[1].total, row_sum);

        // 估算成本（M3-1）：glm-5.3 经尾段兜回有单价 → 成本卡有值；
        // 缺价模型（glm-5.3-flash / claude-sonnet 是否在快照中随上游浮动）计数 ≤2；
        // 趋势成本与汇总成本同乘价逻辑，跨图守恒
        assert!(snap.summary.est_cost.unwrap_or(0.0) > 0.0);
        assert!(snap.summary.missing_price_count <= 2);
        let glm_m = snap.by_model.iter().find(|m| m.label == "glm-5.3").unwrap();
        assert!(glm_m.cost > 0.0);
        let trend_cost: f64 = snap.trend.iter().map(|t| t.cost).sum();
        assert!((trend_cost - snap.summary.est_cost.unwrap()).abs() < 1e-6);

        // 热力图：token 与次数双守恒，坐标合法
        assert_eq!(snap.heatmap.iter().map(|c| c.total).sum::<i64>(), row_sum * 3);
        assert_eq!(snap.heatmap.iter().map(|c| c.calls).sum::<i64>(), 3);
        assert!(snap.heatmap.iter().all(|c| (0..=6).contains(&c.weekday) && (0..=23).contains(&c.hour)));

        // 趋势（all→月桶）：四项与次数守恒
        let t_sum: (i64, i64) = snap
            .trend
            .iter()
            .map(|t| (t.input + t.output + t.cache_read + t.cache_creation, t.calls))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        assert_eq!(t_sum, (row_sum * 3, 3));

        // 下拉选项：只受范围影响
        assert_eq!(snap.options.agents.len(), 2);
        assert_eq!(snap.options.projects.len(), 2);
        assert_eq!(snap.options.models.len(), 3);

        // 维度过滤：agent=zcode → 只剩 2 行
        let z = store.report_snapshot("all", Some(vec!["zcode".to_string()]).as_deref(), None, None, None).unwrap();
        assert_eq!(z.summary.calls, 2);
        assert_eq!(z.summary.total_tokens, row_sum * 2);
        // 项目过滤：projA → zcode s1 + CC s1
        let pa = store.report_snapshot("all", None, Some(vec![r"F:\projA".to_string()]).as_deref(), None, None).unwrap();
        assert_eq!(pa.summary.calls, 2);
        // 模型过滤（大小写不敏感）
        let m = store.report_snapshot("all", None, None, Some(vec!["GLM-5.3".to_string()]).as_deref(), None).unwrap();
        assert_eq!(m.summary.calls, 1);

        // 今日档：now 样本必然落入（本机时区零点 <= now）
        let today = store.report_snapshot("today", None, None, None, None).unwrap();
        assert_eq!(today.summary.calls, 3);
        // 环比基准：today 的上一周期=昨天（样本全在今天，昨期 0 次调用）；all 档无上期
        assert_eq!(today.prev_summary.as_ref().unwrap().calls, 0);
        assert!(snap.prev_summary.is_none());
        // 近 7 天档同量；非法档拒绝
        assert_eq!(store.report_snapshot("7d", None, None, None, None).unwrap().summary.calls, 3);
        assert!(store.report_snapshot("xyz", None, None, None, None).is_none());

        // 按模型首字延迟：三模型各 1 条 ttft=900（样本工厂固定值）
        assert_eq!(snap.ttft_by_model.len(), 3);
        assert!(snap.ttft_by_model.iter().all(|t| t.total == 900 && t.calls == 1));
        // 额度历史：空库无快照 → 空曲线
        assert!(snap.quota_curve.is_empty());
        // 供应商筛选（M3-1 归因口径）：glm → 2 行；anthropic → 1 行；unknown → 空
        let pv = store.report_snapshot("all", None, None, None, Some(vec!["glm".to_string()]).as_deref()).unwrap();
        assert_eq!(pv.summary.calls, 2);
        let pa2 = store.report_snapshot("all", None, None, None, Some(vec!["anthropic".to_string()]).as_deref()).unwrap();
        assert_eq!(pa2.summary.calls, 1);
        let pu = store.report_snapshot("all", None, None, None, Some(vec!["unknown".to_string()]).as_deref()).unwrap();
        assert_eq!(pu.summary.calls, 0);
        // 下拉选项含供应商（归因生成、排序稳定）；趋势思考 token 三桶各 50 守恒
        assert_eq!(snap.options.providers, vec!["anthropic".to_string(), "glm".to_string()]);
        assert_eq!(snap.trend.iter().map(|t| t.reasoning).sum::<i64>(), 150);

        // ===== M1-10 会话中心：追加一个 4 小时前的已结束样本（插在报表断言之后，
        // 不影响上方 report_snapshot 对 3 行样本的守恒断言）=====
        let ended_rows = vec![mk(
            "claude-code",
            "s2",
            "claude-sonnet",
            r"F:\projC",
            now - 4 * 3_600_000,
            None,
            None,
        )];
        store.insert_usage(&ended_rows).unwrap();
        // 会话中心分页：默认最近活动降序、total/页大小、时长与状态列口径（CC 无时长）
        let page1 = store
            .session_page("all", None, None, None, "all", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(page1.total, 4);
        assert_eq!(page1.page_size, SESSION_PAGE_SIZE);
        assert_eq!(page1.rows.len(), 4);
        assert!(page1.rows.windows(2).all(|w| w[0].last_ts >= w[1].last_ts), "最近活动降序");
        let cc_row = page1.rows.iter().find(|r| r.agent == "claude-code" && r.session_id.ends_with("s1")).unwrap();
        assert_eq!(cc_row.duration_ms, None, "CC 转录无时长字段应为 None");
        assert_eq!(cc_row.state.as_deref(), Some("idle"), "sessions 行状态随行下发");
        let zc_row = page1.rows.iter().find(|r| r.session_id == "zcode:s1").unwrap();
        assert_eq!(zc_row.duration_ms, Some(5_000));
        assert_eq!(zc_row.reasoning_tokens, 50, "思考 token 随聚合下发");
        assert_eq!(zc_row.ttft_avg_ms, Some(900), "平均首字随聚合下发");
        // 四项拆解守恒：总量 = 四项之和（样本 1000+200+3000+0）
        assert_eq!(zc_row.input_tokens, 1_000);
        assert_eq!(zc_row.output_tokens, 200);
        assert_eq!(zc_row.cache_read_tokens, 3_000);
        assert_eq!(zc_row.cache_creation_tokens, 0);
        assert_eq!(
            zc_row.total_tokens,
            zc_row.input_tokens + zc_row.output_tokens + zc_row.cache_read_tokens + zc_row.cache_creation_tokens
        );
        // 错误统计：zcode:s2 有限流错误，其余为 0
        let err_row = page1.rows.iter().find(|r| r.session_id == "zcode:s2").unwrap();
        assert_eq!(err_row.errors, 1);
        assert_eq!(err_row.error_types.as_deref(), Some("rate_limited"));
        assert_eq!(zc_row.errors, 0);
        assert_eq!(zc_row.error_types, None);
        // 每页行数：自定义生效、0/负值回退默认、超上限钳 200
        let p2rows = store
            .session_page("all", None, None, None, "all", "", "recent", 2, 0)
            .unwrap();
        assert_eq!(p2rows.rows.len(), 2);
        assert_eq!(p2rows.page_size, 2);
        let p0 = store
            .session_page("all", None, None, None, "all", "", "recent", 0, 0)
            .unwrap();
        assert_eq!(p0.page_size, SESSION_PAGE_SIZE);
        let p999 = store
            .session_page("all", None, None, None, "all", "", "recent", 999, 0)
            .unwrap();
        assert_eq!(p999.page_size, 200);
        // 排序切换：按 token 降序（白名单第二键）
        let by_tokens = store
            .session_page("all", None, None, None, "all", "", "tokens", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert!(by_tokens.rows.windows(2).all(|w| w[0].total_tokens >= w[1].total_tokens));
        // 关键字：命中项目全路径；无匹配归零；LIKE 通配符按字面量转义
        let kw = store
            .session_page("all", None, None, None, "all", "projA", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(kw.total, 2, "projA 两会话命中");
        assert_eq!(
            store.session_page("all", None, None, None, "all", "无此项目", "recent", SESSION_PAGE_SIZE, 0)
                .unwrap()
                .total,
            0
        );
        assert_eq!(
            store.session_page("all", None, None, None, "all", "proj_", "recent", SESSION_PAGE_SIZE, 0)
                .unwrap()
                .total,
            0,
            "_ 已转义为字面量，不当代通配符"
        );
        // 状态档：三个近期 idle 样本=active，4 小时前样本=ended；errored 只剩限流会话
        let active = store
            .session_page("all", None, None, None, "active", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(active.total, 3, "idle 未超 2h 视为进行中");
        let ended = store
            .session_page("all", None, None, None, "ended", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(ended.total, 1);
        assert_eq!(ended.rows[0].session_id, "claude-code:s2");
        let errored = store
            .session_page("all", None, None, None, "errored", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(errored.total, 1);
        assert_eq!(errored.rows[0].session_id, "zcode:s2");
        // 白名单拒绝：非法状态档/排序键
        assert!(store.session_page("all", None, None, None, "xyz", "", "recent", SESSION_PAGE_SIZE, 0).is_none());
        assert!(store.session_page("all", None, None, None, "all", "", "xyz", SESSION_PAGE_SIZE, 0).is_none());
        // 翻页：offset 越界返回空页但 total 不变
        let page2 = store
            .session_page("all", None, None, None, "all", "", "recent", SESSION_PAGE_SIZE, SESSION_PAGE_SIZE)
            .unwrap();
        assert_eq!(page2.total, 4);
        assert!(page2.rows.is_empty());
        // 过滤联动：agent=zcode → 会话表只剩 2 个
        let zpage = store
            .session_page("all", Some("zcode"), None, None, "all", "", "recent", SESSION_PAGE_SIZE, 0)
            .unwrap();
        assert_eq!(zpage.total, 2);

        // 会话详情（M1-11）：流水倒序 + 状态事件双口径（命名空间 id 与原始 id 均命中）
        let detail = store.session_detail("zcode:s1");
        assert_eq!(detail.calls.len(), 1);
        assert_eq!(detail.calls[0].input_tokens, 1_000);
        assert_eq!(detail.calls[0].duration_ms, Some(5_000));
        // 真实总数与列表长度一致（未截断时相等；截断口径由 LIMIT 上限保证，此处验证 COUNT 正确性）
        assert_eq!(detail.calls_total, 1);
        assert_eq!(detail.events_total, 0);
        store.insert_status_event("claude-code", Some("s1"), "SessionStart", "{}", now);
        store.insert_status_event("claude-code", Some("claude-code:s1"), "UserPromptSubmit", "{}", now + 1);
        // zcode:s1 只能靠原始 id 命中 raw 事件；claude-code:s1 双口径（命名空间 id + 原始 id）各命中
        assert_eq!(store.session_detail("zcode:s1").events.len(), 1, "原始 id 口径命中");
        let cc_detail = store.session_detail("claude-code:s1");
        assert_eq!(
            cc_detail.events.len(),
            2,
            "原始 id 与命名空间 id 均可命中"
        );
        assert_eq!(cc_detail.events_total, 2, "状态事件总数同样兼容双口径");

        // R2 汇总扩展：时长合计/平均首字/思考占比原料（样本 ttft 全 900、reasoning 全 50）
        assert_eq!(snap.summary.duration_ms, Some(5_000));
        assert_eq!(snap.summary.ttft_avg_ms, Some(900));
        assert_eq!(snap.summary.reasoning_tokens, 150);
        assert_eq!(snap.summary.billable_tokens, 1_200 * 3);
        // R2 趋势行时长：仅 zcode s1 贡献 5000
        let trend_dur: i64 = snap.trend.iter().filter_map(|t| t.duration_ms).sum();
        assert_eq!(trend_dur, 5_000);
        // R2 错误分布：仅 1 条 rate_limited
        assert_eq!(snap.by_error.len(), 1);
        assert_eq!(snap.by_error[0].label, "rate_limited");
        assert_eq!(snap.by_error[0].calls, 1);

        // M1-10 CSV 导出：BOM 表头 + 4 行会话（最近活动降序），状态/过滤与页面同口径
        let csv = store
            .build_sessions_csv("all", None, None, None, "all", "", "recent")
            .unwrap();
        assert!(csv.starts_with('\u{feff}'));
        assert_eq!(csv.lines().count(), 5, "表头 + 4 行");
        assert!(csv.contains("zcode:s1"));
        assert!(csv.contains("claude-code:s2"));
        let zc_csv = store
            .build_sessions_csv("all", Some("zcode"), None, None, "all", "", "recent")
            .unwrap();
        assert_eq!(zc_csv.lines().count(), 3, "过滤后 2 行会话");
        assert!(!zc_csv.contains("claude-code"));
        assert!(store.build_sessions_csv("xyz", None, None, None, "all", "", "recent").is_none());
        assert!(store.build_sessions_csv("all", None, None, None, "xyz", "", "recent").is_none());
        let ended_csv = store
            .build_sessions_csv("all", None, None, None, "ended", "", "recent")
            .unwrap();
        assert_eq!(ended_csv.lines().count(), 2, "状态档过滤生效");
        let _ = std::fs::remove_file(&path);
    }

    /// CSV 字段转义（RFC 4180）：逗号/引号/换行触发包裹，引号双写
    #[test]
    fn test_csv_cell() {
        assert_eq!(csv_cell("普通"), "普通");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("说\"引号"), "\"说\"\"引号\"");
        assert_eq!(csv_cell("换\n行"), "\"换\n行\"");
    }

    /// M3-1：单价覆盖表读取生效 + 成本与手算一致（验收硬要求）。
    /// 覆盖 glm-5.3 单价为已知值，灌固定 token 数，断言四桶乘价结果精确
    #[test]
    fn test_price_override_and_manual_cost() {
        let path = tmp_db("price");
        let store = Store::open(&path).unwrap();
        // 用户覆盖行（编辑 UI 列后续池，此处直插验证读取链路）
        {
            let conn = store.lock_conn();
            conn.execute(
                "INSERT INTO model_price_overrides
                   (model_lower, input_cost_per_token, output_cost_per_token,
                    cache_read_cost, cache_creation_cost, updated_at)
                 VALUES ('glm-5.3', 0.000003, 0.000015, 0.0000003, 0.00000375, 0)",
                [],
            )
            .unwrap();
        }
        // 用量：1M 输入 + 0.5M 输出 + 2M 缓存读 + 0.4M 缓存写（模型名大小写混合）
        let mut row = sample_usage(1_000);
        row.model = "GLM-5.3".into();
        row.input_tokens = Some(1_000_000);
        row.output_tokens = Some(500_000);
        row.cache_read_tokens = Some(2_000_000);
        row.cache_creation_tokens = Some(400_000);
        row.duration_ms = None;
        row.ttft_ms = None;
        store.insert_usage(&[row]).unwrap();        let snap = store.report_snapshot("all", None, None, None, None).unwrap();
        // 手算：1M×3e-6 + 0.5M×1.5e-5 + 2M×3e-7 + 0.4M×3.75e-6 = 3+7.5+0.6+1.5 = 12.6 美元
        let est = snap.summary.est_cost.expect("覆盖表命中应可计价");
        assert!((est - 12.6).abs() < 1e-9, "成本与手算不一致：{est}");
        assert_eq!(snap.summary.missing_price_count, 0);
        assert_eq!(snap.by_model.len(), 1);
        assert!((snap.by_model[0].cost - 12.6).abs() < 1e-9);
        assert!((snap.trend.iter().map(|t| t.cost).sum::<f64>() - 12.6).abs() < 1e-9);
        let _ = std::fs::remove_file(&path);
    }

    /// M3-1：供应商维度按模型名归因——存量 provider 列旧口径（alibaba/NULL）
    /// 不参与分组，新旧数据统一归并；筛选与下拉选项同口径
    #[test]
    fn test_provider_attribution_ignores_legacy_column() {
        let path = tmp_db("attr");
        let store = Store::open(&path).unwrap();
        let mk = |sid: &str, model: &str, provider: Option<&str>, ts: i64| UsageRow {
            session_id: format!("zcode:{sid}"),
            agent: "zcode".into(),
            model: model.into(),
            provider: provider.map(String::from),
            ts,
            input_tokens: Some(100),
            output_tokens: None,
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            duration_ms: None,
            ttft_ms: None,
            error_type: None,
            source_id: Some(format!("attr:{sid}")),
            is_background: false,
        };
        store.insert_usage(&[
            // 存量旧口径：qwen 系曾写入 "alibaba"
            mk("a", "qwen3-max", Some("alibaba"), 1_000),
            // 新口径写入 "qwen"
            mk("b", "qwq-32b", Some("qwen"), 2_000),
            // 前缀规则与快照兜底都未命中 → unknown 组
            mk("c", "mystery-model-xyz", None, 3_000),
        ]).unwrap();
        let snap = store.report_snapshot("all", None, None, None, None).unwrap();
        let qwen = snap.by_provider.iter().find(|p| p.label == "qwen").expect("qwen 组应存在");
        assert_eq!(qwen.total, 200, "新旧 provider 口径的行应归并同组");
        assert_eq!(qwen.calls, 2);
        assert!(snap.by_provider.iter().any(|p| p.label == "unknown"), "归因失败归 unknown");
        // 筛选联动：qwen → 2 行；unknown → 1 行
        let pq = store.report_snapshot("all", None, None, None, Some(vec!["qwen".to_string()]).as_deref()).unwrap();
        assert_eq!(pq.summary.calls, 2);
        let pu = store.report_snapshot("all", None, None, None, Some(vec!["unknown".to_string()]).as_deref()).unwrap();
        assert_eq!(pu.summary.calls, 1);
        // 下拉选项（BTreeSet 排序，unknown 恒排尾）
        assert_eq!(snap.options.providers, vec!["qwen".to_string(), "unknown".to_string()]);
        let _ = std::fs::remove_file(&path);
    }
}
