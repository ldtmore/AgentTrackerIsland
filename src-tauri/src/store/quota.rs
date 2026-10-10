// 额度与余额域：快照写入/最新快照/历史曲线（抽稀）/pace 采样点
// （2026-10-03 审查拆分：自 mod.rs 纯移动，方法体逐字节不变；领域内聚，
// Store 方法经 inherent impl 就地扩展，外部调用路径零变化）

use super::*;

/// 额度曲线单序列点数上限：超出按等步长抽样（保留首尾），
/// 防止"全部"档把数月历史一次性塞进一次 IPC（5 分钟一条，两年约 20 万点）
const QUOTA_MAX_POINTS: usize = 4000;

impl Store {
    /// 插入额度快照（M3-2 起带实例归属；account_id 为空 = 迁移前的历史口径）
    pub fn insert_quota(&self, row: &QuotaRow) {
        let conn = self.lock_conn();
        let res = conn.execute(
            "INSERT INTO quota_snapshots(provider, window_kind, used_percent, used_tokens, reset_at, fetched_at, account_id)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![row.provider, row.window_kind, row.used_percent, row.used_tokens, row.reset_at, row.fetched_at, row.account_id],
        );
        if let Err(e) = &res {
            log::warn!("[存储] 额度快照写入失败（{} {}%，下轮重查补上）：{e}", row.window_kind, row.used_percent.unwrap_or(0.0) as i64);
        }
        // 写库成功才同步最新快照缓存（latest_quotas 快路径的数据源，2026-10-10
        // 审查新增）；失败时缓存维持旧值与库一致，下轮重查自然补上。缓存未初始化
        // （None）时跳过——首轮回源会连本条一起带上
        if res.is_ok() {
            if let Some(map) = self
                .quota_latest_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut()
            {
                map.insert(
                    (row.provider.clone(), row.window_kind.clone(), row.account_id.clone()),
                    row.clone(),
                );
            }
        }
    }

    /// 插入货币余额快照（Balance 口径）
    pub fn insert_balance(&self, row: &BalanceRow) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute(
            "INSERT INTO balance_snapshots(account_id, currency, total, granted, available, fetched_at)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![row.account_id, row.currency, row.total, row.granted, row.available, row.fetched_at],
        ) {
            log::warn!("[存储] 余额快照写入失败（下轮重查补上）：{e}");
        }
    }

    /// 某实例最新一条余额快照（实例卡摘要/托盘告急口径用）
    pub fn latest_balance(&self, account_id: &str) -> Option<BalanceRow> {
        let conn = self.lock_conn();
        // ORDER BY 对齐 idx_balance_account(account_id, fetched_at)（2026-09-29
        // 审查修复 #4：原 ORDER BY id DESC 与索引序不符，优化器需回表全取或
        // rowid 倒序早停，计划不稳定；id 作并列兜底保证语义同"最新一行"）
        conn.query_row(
            "SELECT account_id, currency, total, granted, available, fetched_at
             FROM balance_snapshots WHERE account_id = ?1
             ORDER BY fetched_at DESC, id DESC LIMIT 1",
            params![account_id],
            |r| {
                Ok(BalanceRow {
                    account_id: r.get(0)?,
                    currency: r.get(1)?,
                    total: r.get(2)?,
                    granted: r.get(3)?,
                    available: r.get(4)?,
                    fetched_at: r.get(5)?,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    }

    /// 每个实例+窗口的最新额度快照（M3-2 起同厂商多实例各自独立）。
    /// 分组键 = (provider, window_kind, IFNULL(account_id))：存量 NULL 行自成一组
    /// 不丢历史；已回填行按实例分组。
    /// 2026-09-29 审查修复 #4：原相关子查询对每外层行执行一次且 IFNULL 包裹列
    /// 使 idx_quota_account 失效——每 tick（1~10s）近全表扫描，快照历史增长后
    /// 线性劣化；改窗口函数按分区取最新一行，配合 0008 的表达式索引
    /// idx_quota_latest(provider, window_kind, IFNULL(account_id,''), id) 流式处理
    /// 2026-10-10 审查修复：窗口函数无提前终止，行数仍随保留期线性增长（默认
    /// 365 天 × 每实例 2 窗口 × 5min 采样 ≈ 每实例每年 20 万行），而本查询挂在
    /// 聚合 tick（活跃档 1s）上且持全局连接锁——改为进程内缓存快路径直读，
    /// insert_quota 同步维护、清理/回填改写历史分组时整体失效，仅冷启动首轮
    /// 走 latest_quotas_scan 全量回源一次
    pub fn latest_quotas(&self) -> Vec<QuotaRow> {
        let cache = self
            .quota_latest_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(map) = cache.as_ref() {
            return map.values().cloned().collect();
        }
        drop(cache);
        let rows = self.latest_quotas_scan();
        let mut map = std::collections::HashMap::with_capacity(rows.len());
        for r in &rows {
            map.insert(
                (r.provider.clone(), r.window_kind.clone(), r.account_id.clone()),
                r.clone(),
            );
        }
        *self
            .quota_latest_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(map);
        rows
    }

    /// 全量扫描各分组最新一行（仅缓存回源时调用；语义同缓存分组约定）
    fn latest_quotas_scan(&self) -> Vec<QuotaRow> {
        let conn = self.lock_conn();
        let mut stmt = match conn.prepare(
            "SELECT provider, window_kind, used_percent, used_tokens, reset_at, fetched_at, account_id
             FROM (
               SELECT provider, window_kind, used_percent, used_tokens, reset_at,
                      fetched_at, account_id,
                      ROW_NUMBER() OVER (
                        PARTITION BY provider, window_kind, IFNULL(account_id, '')
                        ORDER BY id DESC
                      ) AS rn
               FROM quota_snapshots
             ) WHERE rn = 1",
        ) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("[存储] latest_quotas 查询失败（本轮返回空降级）：{e}");
                return vec![]
            },
        };
        let rows = stmt.query_map([], |r| {
            Ok(QuotaRow {
                provider: r.get(0)?,
                window_kind: r.get(1)?,
                used_percent: r.get(2)?,
                used_tokens: r.get(3)?,
                reset_at: r.get(4)?,
                fetched_at: r.get(5)?,
                account_id: r.get(6)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] latest_quotas 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        }
    }

    /// 存量快照回填实例归属（GLM 迁移一次性：0007 前的 glm 快照全部划归迁移实例）
    pub fn backfill_quota_account(&self, provider: &str, account_id: &str) -> u64 {
        let conn = self.lock_conn();
        let n = conn.execute(
            "UPDATE quota_snapshots SET account_id = ?1
             WHERE provider = ?2 AND account_id IS NULL",
            params![account_id, provider],
        )
        .unwrap_or_else(|e| {
            log::warn!("[存储] 存量快照回填 account_id 失败（provider={provider}）：{e}");
            0
        }) as u64;
        if n > 0 {
            // 回填改写了分组键（NULL → 实例归属），最新快照缓存整体失效下轮回源
            //（2026-10-10 审查新增，与 latest_quotas 缓存配套）
            *self
                .quota_latest_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
        n
    }

    /// 额度消耗历史采样（额度曲线用）：fetched_at 升序、跳过无百分比的历史行；
    /// 超上限时按等步长抽样并保留首尾——锯齿（用满→重置）形态靠首点极值不丢
    pub fn quota_history(&self, cutoff_ms: i64) -> Vec<QuotaPoint> {
        let conn = self.lock_conn();
        let Ok(mut stmt) = conn.prepare(
            "SELECT provider, IFNULL(account_id,''), window_kind, fetched_at, used_percent
             FROM quota_snapshots
             WHERE fetched_at >= ?1 AND used_percent IS NOT NULL
             ORDER BY fetched_at ASC",
        ) else {
            return vec![];
        };
        let rows = stmt.query_map([cutoff_ms], |r| {
            // 空串归一 None：0007 之前的存量快照无实例归属，前端按"默认实例"分线
            let acc: String = r.get(1)?;
            Ok(QuotaPoint {
                provider: r.get(0)?,
                account_id: if acc.is_empty() { None } else { Some(acc) },
                window_kind: r.get(2)?,
                fetched_at: r.get(3)?,
                used_percent: r.get(4)?,
            })
        });
        let all: Vec<QuotaPoint> = match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] quota_history 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        };
        if all.len() <= QUOTA_MAX_POINTS {
            return all;
        }
        // 按序列（厂商×实例×窗口）分组后各自等步长抽样（2026-09-29 审查修复 #8：
        // 原全局抽样在多实例下会把点数少的线抽稀甚至抽空；组内语义与原实现一致，
        // 步长向上取整、末点必留）
        let mut groups: std::collections::BTreeMap<(String, Option<String>, String), Vec<QuotaPoint>> =
            Default::default();
        for p in all {
            groups
                .entry((p.provider.clone(), p.account_id.clone(), p.window_kind.clone()))
                .or_default()
                .push(p);
        }
        let mut out = Vec::new();
        for (_, mut pts) in groups {
            if pts.len() <= QUOTA_MAX_POINTS {
                out.append(&mut pts);
                continue;
            }
            let n = pts.len();
            let step = n.div_ceil(QUOTA_MAX_POINTS);
            // 末点必留（四轮审查，兑现上行注释承诺）：pts 按时间升序，末点＝最新
            // 采样——纯等步长抽样在 (n-1)%step≠0 时会把曲线末端抽成旧值
            out.extend(
                pts.into_iter()
                    .enumerate()
                    .filter(|(i, _)| i % step == 0 || *i == n - 1)
                    .map(|(_, p)| p),
            );
        }
        out
    }

    /// pace 近期采样点（#21）：某实例某窗口近 cutoff 内的 (fetched_at, used_percent)
    /// 升序原始点（不抽稀——pace 只要近期两三个点，走 idx_quota_latest 分区序）。
    /// 供 compute_pace 求斜率
    pub fn quota_pace_points(
        &self,
        account_id: &str,
        window_kind: &str,
        cutoff_ms: i64,
    ) -> Vec<(i64, f64)> {
        let conn = self.lock_conn();
        let Ok(mut stmt) = conn.prepare(
            "SELECT fetched_at, used_percent FROM quota_snapshots
             WHERE account_id = ?1 AND window_kind = ?2 AND used_percent IS NOT NULL
               AND fetched_at >= ?3
             ORDER BY fetched_at ASC",
        ) else {
            return vec![];
        };
        let rows = stmt.query_map(
            rusqlite::params![account_id, window_kind, cutoff_ms],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)),
        );
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] quota_pace_points 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        }
    }

}
