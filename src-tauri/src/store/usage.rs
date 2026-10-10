// 采集写入与会话查询域：水位/会话元数据/用量写入/最近调用与错误回填/清理
// （2026-10-03 审查拆分：自 mod.rs 纯移动，方法体逐字节不变；领域内聚，
// Store 方法经 inherent impl 就地扩展，外部调用路径零变化）

use super::*;

impl Store {
    /// 读取某 Agent 的采集水位（时间戳，毫秒）；无记录返回 0
    pub fn get_watermark(&self, agent: &str) -> i64 {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT last_ts FROM watermarks WHERE agent = ?1",
            params![agent],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or(0)
    }

    /// 更新采集水位（仅前进，不回退）
    pub fn set_watermark(&self, agent: &str, ts: i64) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute(
            "INSERT INTO watermarks(agent, last_ts) VALUES(?1, ?2)
             ON CONFLICT(agent) DO UPDATE SET last_ts = MAX(last_ts, excluded.last_ts)",
            params![agent, ts],
        ) {
            log::warn!("[存储] 水位写入失败（agent={agent}，下轮幂等重写）：{e}");
        }
    }

    /// upsert 会话元数据（首见时间不覆盖，最新状态全量刷新）
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_session(
        &self,
        id: &str,
        agent: &str,
        provider: Option<&str>,
        model: Option<&str>,
        project_dir: Option<&str>,
        title: Option<&str>,
        last_seen_at: i64,
        state: &str,
        state_reason: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute(
            "INSERT INTO sessions(id, agent, provider, model, project_dir, title,
                                  first_seen_at, last_seen_at, state, state_reason)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET
               provider = COALESCE(excluded.provider, provider),
               model    = COALESCE(excluded.model, model),
               project_dir = COALESCE(excluded.project_dir, project_dir),
               title = COALESCE(excluded.title, title),
               last_seen_at = MAX(last_seen_at, excluded.last_seen_at),
               state = excluded.state,
               state_reason = excluded.state_reason",
            params![id, agent, provider, model, project_dir, title, last_seen_at, state, state_reason],
        ) {
            log::warn!("[存储] 会话元数据写入失败（id={id}，下轮重写）：{e}");
            // Err 透传给调用方：service 层签名缓存只在成功后推进（否则该行
            // 持续缺写直到字段变化，库/内存口径分裂）
            return Err(anyhow::anyhow!("会话元数据写入失败：{e}"));
        }
        Ok(())
    }

    /// 幂等插入用量流水（2026-09-21 双计根治）：
    /// 幂等键 = (agent, session_id, source_id)——source_id 是真实消息身份，
    /// CC 流式复制快照行（同消息 timestamp 各异）在增量采集里靠它归并；
    /// 冲突时：普通行仅当新行四项合计更大才整行覆盖（"保留最大快照"口径），
    /// 后台校准行（存量 is_background=1）无条件覆盖——差值随 assistant 明细
    /// 增长会缩小，必须跟随重算值而非保留历史最大。
    /// source_id 为 NULL 的行不触发冲突（SQLite NULL≠NULL），多行共存仅作防御兜底；
    /// 返回实际变更行数（新插入或覆盖）。
    /// 失败语义（红线③防线，2026-10-03 四轮审查）：事务开启/提交失败返回 Err——
    /// 调用方（service）必须跳过水位推进，否则本批数据回滚后水位已前进，
    /// 早于「水位−60s」的行从此不再被采集＝静默永久丢数据；
    /// 单行写失败仍记 warn 跳过（该行 ts 照常参与水位）——单行失败多为确定性
    /// 坏行（Schema 漂移/约束冲突），若因它阻断水位会让整条采集管线永久停摆，
    /// 两害取轻：丢一行好过丢一个 Agent 的全部后续数据
    pub fn insert_usage(&self, rows: &[UsageRow]) -> anyhow::Result<usize> {
        let mut conn = self.lock_conn();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                log::warn!("insert_usage 开启事务失败（本轮 {} 行放弃，下轮重采）：{e}", rows.len());
                return Err(anyhow::anyhow!("insert_usage 开启事务失败：{e}"));
            }
        };
        let mut changed = 0usize;
        for r in rows {
            let n = tx.execute(
                "INSERT INTO usage_records(
                   session_id, agent, model, provider, ts,
                   input_tokens, output_tokens, reasoning_tokens,
                   cache_read_tokens, cache_creation_tokens,
                   duration_ms, ttft_ms, error_type, source_id, is_background)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                 ON CONFLICT(agent, session_id, source_id) DO UPDATE SET
                   input_tokens = excluded.input_tokens,
                   output_tokens = excluded.output_tokens,
                   reasoning_tokens = excluded.reasoning_tokens,
                   cache_read_tokens = excluded.cache_read_tokens,
                   cache_creation_tokens = excluded.cache_creation_tokens,
                   duration_ms = excluded.duration_ms,
                   ttft_ms = excluded.ttft_ms,
                   error_type = excluded.error_type,
                   ts = excluded.ts
                 WHERE is_background = 1
                    OR (COALESCE(excluded.input_tokens,0) + COALESCE(excluded.output_tokens,0)
                      + COALESCE(excluded.cache_read_tokens,0) + COALESCE(excluded.cache_creation_tokens,0))
                       >
                       (COALESCE(input_tokens,0) + COALESCE(output_tokens,0)
                      + COALESCE(cache_read_tokens,0) + COALESCE(cache_creation_tokens,0))",
                params![
                    r.session_id, r.agent, r.model, r.provider, r.ts,
                    r.input_tokens, r.output_tokens, r.reasoning_tokens,
                    r.cache_read_tokens, r.cache_creation_tokens,
                    r.duration_ms, r.ttft_ms, r.error_type,
                    r.source_id, r.is_background as i64
                ],
            );
            match n {
                Ok(v) => changed += v,
                Err(e) => log::warn!("insert_usage 单行写库失败（ts={} model={}）：{e}", r.ts, r.model),
            }
        }
        if let Err(e) = tx.commit() {
            log::warn!("insert_usage 提交事务失败（本轮全部回滚，下轮重采）：{e}");
            return Err(anyhow::anyhow!("insert_usage 提交事务失败：{e}"));
        }
        Ok(changed)
    }

    /// 插入原始状态事件（hooks/采集审计）。失败留痕：该表无幂等键，hook 偏移
    /// 持久化失败的旧偏移重放会插重复审计行——静默吞错会让时间线无痕失真
    pub fn insert_status_event(&self, agent: &str, session_id: Option<&str>, hook: &str, payload: &str, ts: i64) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute(
            "INSERT INTO status_events(agent, session_id, hook, payload, ts) VALUES(?1,?2,?3,?4,?5)",
            params![agent, session_id, hook, payload, ts],
        ) {
            log::warn!("[存储] 状态事件写入失败（agent={agent} hook={hook}）：{e}");
        }
    }

    /// 某会话累计 token（input+output）。单会话点查，仅供测试与工具用途；
    /// 展示层走 session_usage_breakdown 批量版（账单口径含缓存，审查 2.2.2 治理 N+1）
    pub fn session_usage_total(&self, session_id: &str) -> i64 {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)),0)
             FROM usage_records WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    /// 批量：一组会话的用量四项拆解（2026-09-18 展示改造，替代旧的 input+output
    /// 单总和版 session_usage_totals）。一次 GROUP BY 替代每会话一次点查
    /// （审查 2.2.2），前端 tooltip 拆解与总消耗（账单口径）共用本查询
    pub fn session_usage_breakdown(
        &self,
        session_ids: &[String],
    ) -> std::collections::HashMap<String, TokenBreakdown> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = self.lock_conn();
        // 占位符仅由内部拼接（元素为自产会话 id），无外部输入
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT session_id,
                    COALESCE(SUM(COALESCE(input_tokens,0)),0),
                    COALESCE(SUM(COALESCE(output_tokens,0)),0),
                    COALESCE(SUM(COALESCE(cache_read_tokens,0)),0),
                    COALESCE(SUM(COALESCE(cache_creation_tokens,0)),0)
             FROM usage_records WHERE session_id IN ({placeholders}) GROUP BY session_id"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            log::warn!("session_usage_breakdown 查询准备失败");
            return out;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                TokenBreakdown {
                    input: r.get(1)?,
                    output: r.get(2)?,
                    cache_read: r.get(3)?,
                    cache_creation: r.get(4)?,
                },
            ))
        });
        if let Ok(it) = rows {
            for (sid, breakdown) in it.filter_map(|x| x.ok()) {
                out.insert(sid, breakdown);
            }
        }
        out
    }

    /// 今日用量汇总（账单口径四项相加）： 本机今日零点之后的 token 总量与调用次数。
    /// 供胶囊/面板"今日"口径展示（2026-09-18 展示改造，替代误导性的全历史"累计"）。
    /// 调用次数不含后台校准行（is_background，cost-state 差值非真实调用）
    pub fn today_usage(&self, day_start_ms: i64) -> (i64, i64) {
        let conn = self.lock_conn();
        let result: rusqlite::Result<(i64, i64)> = conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)
                    +COALESCE(cache_read_tokens,0)+COALESCE(cache_creation_tokens,0)),0),
                    COALESCE(SUM(NOT is_background),0)
             FROM usage_records WHERE ts >= ?1",
            params![day_start_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        );
        match result {
            Ok(v) => v,
            Err(e) => {
                log::warn!("today_usage 查询失败（按 0 处理）：{e}");
                (0, 0)
            }
        }
    }

    /// 批量：每个会话最近一次出错的错误类型与时间（无错误的会话不在结果中）。
    /// 供面板"出错 · 限流/额度耗尽"等具体原因展示（2026-09-18 展示改造）
    pub fn latest_session_errors(
        &self,
        session_ids: &[String],
    ) -> std::collections::HashMap<String, (String, i64)> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = self.lock_conn();
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        // 与 latest_session_models 同构：窗口函数取每会话最近一条有 error_type 的记录
        let sql = format!(
            "SELECT session_id, error_type, ts FROM (
               SELECT session_id, error_type, ts,
                      ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY ts DESC) AS rn
               FROM usage_records
               WHERE session_id IN ({placeholders}) AND error_type IS NOT NULL
             ) WHERE rn = 1"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            log::warn!("latest_session_errors 查询准备失败");
            return out;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        });
        if let Ok(it) = rows {
            for (sid, err, ts) in it.filter_map(|x| x.ok()) {
                out.insert(sid, (err, ts));
            }
        }
        out
    }

    /// 批量：每个会话最近一次调用所用模型（Claude Code scan 阶段拿不到 model，
    /// 展示时兜底回填）。窗口函数取每会话 ts 最大一行，替代逐会话点查（审查 2.2.2）。
    /// 排除后台校准行：cost-state 差值行的模型（如 flash）不代表会话主模型
    pub fn latest_session_models(&self, session_ids: &[String]) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = self.lock_conn();
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT session_id, model FROM (
               SELECT session_id, model,
                      ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY ts DESC) AS rn
               FROM usage_records WHERE session_id IN ({placeholders}) AND is_background = 0
             ) WHERE rn = 1"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            log::warn!("latest_session_models 查询准备失败");
            return out;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        });
        if let Ok(it) = rows {
            for (sid, model) in it.filter_map(|x| x.ok()) {
                out.insert(sid, model);
            }
        }
        out
    }

    /// 每会话最近一次调用时间（自库 usage_records 的 MAX(ts)）。
    /// CC 未装 hooks 时 scan 只有文件 mtime——ai-title/file-history 等非对话行
    /// 追加也会推高 mtime，喂给状态机会产生「假 working」（M1-12 待议区遗留）；
    /// service 层以此查询结果覆盖启发式活动时间（后台行也是真实调用，不排除）
    pub fn latest_call_ts(&self, session_ids: &[String]) -> std::collections::HashMap<String, i64> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = self.lock_conn();
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT session_id, MAX(ts) FROM usage_records
             WHERE session_id IN ({placeholders}) GROUP BY session_id"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            log::warn!("latest_call_ts 查询准备失败");
            return out;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        });
        if let Ok(it) = rows {
            for (sid, ts) in it.filter_map(|x| x.ok()) {
                out.insert(sid, ts);
            }
        }
        out
    }

    /// 查会话元数据（project_dir/agent），跳转窗口用
    pub fn get_session_meta(&self, id: &str) -> Option<(String, Option<String>)> {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT agent, project_dir FROM sessions WHERE id = ?1",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// 某会话某模型的真实调用（非后台）四项用量合计。
    /// cost-state 校准（2026-09-21）用：后台差值 = cost 累计快照 − 本值（下限 0）
    pub fn assistant_model_total(&self, session_id: &str, model_lower: &str) -> i64 {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)
                    +COALESCE(cache_read_tokens,0)+COALESCE(cache_creation_tokens,0)),0)
             FROM usage_records
             WHERE session_id = ?1 AND is_background = 0 AND LOWER(model) = ?2",
            params![session_id, model_lower],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    /// 批量：一组会话已入库的标题（sessions 表持久层）。
    /// 岛面板快照兜底用——CC 标题只随增量采集入库，快照不能依赖"本轮恰好采到"，
    /// 否则文件无新行时标题丢失回退目录名（2026-09-21 实测踩中）
    pub fn session_titles(&self, session_ids: &[String]) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = self.lock_conn();
        let placeholders = session_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, title FROM sessions
             WHERE id IN ({placeholders}) AND title IS NOT NULL AND title != ''"
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            log::warn!("session_titles 查询准备失败");
            return out;
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        });
        if let Ok(it) = rows {
            for (sid, title) in it.filter_map(|x| x.ok()) {
                out.insert(sid, title);
            }
        }
        out
    }

    /// Claude Code 用量行（M3-12 本地推算数据源）：agent='claude-code'（采集层
    /// 写入字面量，见 collector/claude_code.rs id()——曾误写 'claude' 致推算恒 0）
    /// 且 ts ≥ since，返回 (ts, 四类 token 合计) 按 ts 升序。token 口径与
    /// ccusage/Maciek 一致：input＋output＋cache_creation＋cache_read
    /// （不含 reasoning——Claude 无此分项）
    pub fn claude_usage_rows(&self, since: i64) -> anyhow::Result<Vec<(i64, i64)>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT ts, COALESCE(input_tokens,0)+COALESCE(output_tokens,0)
                   +COALESCE(cache_creation_tokens,0)+COALESCE(cache_read_tokens,0)
             FROM usage_records WHERE agent='claude-code' AND ts>=? ORDER BY ts",
        )?;
        let rows = stmt
            .query_map([since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 清空采集水位（0003 重建流程：配合已清空的用量表，触发采集层全量回溯）
    pub fn clear_watermarks(&self) {
        let conn = self.lock_conn();
        if let Err(e) = conn.execute("DELETE FROM watermarks", []) {
            log::warn!("[存储] 水位清空失败（重建流程）：{e}");
        }
    }

    /// 数据清理：删除 before_ts 之前的用量/快照/事件/会话行（设置页滚动周期用；
    /// M3-3 起余额快照有生产者一并纳入；2026-09-28 起 sessions 表纳入——
    /// 「保留 N 个月」语义自然涵盖会话行，无豁免理由。活跃会话的 last_seen_at
    /// 每轮 tick 都被 upsert 刷新不会误删；被删的都是真·长期不活跃行。
    /// sessions 本质是可重建缓存（90 天窗口内 scan 自动回填），清理零数据损失）
    pub fn cleanup_older_than(&self, before_ts: i64) -> u64 {
        let mut total = 0usize;
        // 额度快照有删减时需失效最新快照缓存（2026-10-10 审查新增）：实例删除后
        // 不再上报，其「最新」一条快照会随保留期到期被清掉——缓存若不失效会永久
        // 返回已不存在的行
        let mut quota_purged = false;
        // 分批删除（2026-10-03 审查修复）：单语句全量 DELETE 在「长保留周期改短」
        // 首轮会一次删除全部超期数据，长持写锁阻塞 10s 采集写入 tick；子查询
        // LIMIT 5000 分批后每批瞬时完成。id 为主键（rowid 别名），IN 子查询走索引。
        // 批间释放 Mutex（四轮审查修正）：分批只在 SQLite 层让出写锁还不够——
        // lock_conn 的 MutexGuard 若活到函数末尾，清理全程（首轮可达数十批）
        // 会把全部采集/查询线程挡在 App 层互斥量上，岛刷新冻结数秒；
        // 每批独立短锁，批与批之间其他线程正常进出
        for (table, col) in [
            ("usage_records", "ts"),
            ("quota_snapshots", "fetched_at"),
            ("balance_snapshots", "fetched_at"),
            ("status_events", "ts"),
        ] {
            let sql = format!("DELETE FROM {table} WHERE id IN (SELECT id FROM {table} WHERE {col} < ?1 LIMIT 5000)");
            loop {
                let batch = {
                    let conn = self.lock_conn();
                    conn.execute(&sql, params![before_ts])
                };
                match batch {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n;
                        if table == "quota_snapshots" {
                            quota_purged = true;
                        }
                    }
                    Err(e) => {
                        log::warn!("[存储] 清理 {table} 失败（本轮跳过该表，下轮重试）：{e}");
                        break;
                    }
                }
            }
        }
        if quota_purged {
            *self
                .quota_latest_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
        // sessions 行数＝会话数（千级），单语句瞬时完成，无需分批；
        // 最近活动时间兜底 first_seen（last_seen 理论非空，防御 NULL）
        let sessions_del = {
            let conn = self.lock_conn();
            conn.execute(
                "DELETE FROM sessions WHERE COALESCE(last_seen_at, first_seen_at, 0) < ?1",
                params![before_ts],
            )
        };
        match sessions_del {
            Ok(n) => total += n,
            Err(e) => log::warn!("[存储] 清理 sessions 失败（下轮重试）：{e}"),
        }
        // WAL 截断回收（2026-10-03 审查修复）：WAL 高水位只增不清（实测曾 5.0MB
        // ＞主库 4.5MB），清理节拍后截断归还磁盘空间。有并发读者时 TRUNCATE 会
        // 让位失败——无害（下个清理日再截），不视为错误
        let checkpoint = {
            let conn = self.lock_conn();
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        };
        if let Err(e) = checkpoint {
            log::debug!("[存储] WAL 截断未完成（并发占用，下个清理日再试）：{e}");
        }
        total as u64
    }
    /// 近 2 小时内有活动的会话数（本地 API /v1/status 的活跃会话概数；
    /// 与展示层 ENDED_AFTER_MS 同口径的轻查询）。
    /// 2026-10-03 审查修复：原 SQL 引用幽灵列 last_usage_at（该列只存在于采集层
    /// 内存结构 SessionInfo，从未落库）——报错被 unwrap_or(0) 吞掉导致恒返 0；
    /// 现按 COALESCE(last_seen_at, first_seen_at) 口径（与 cleanup 同源，活跃
    /// 会话每轮 tick 刷新 last_seen_at），且查询失败记日志不再静默
    pub fn recent_session_count(&self) -> i64 {
        let conn = self.lock_conn();
        match conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE COALESCE(last_seen_at, first_seen_at, 0) >= ?1",
            [now_ms() - ENDED_AFTER_MS],
            |r| r.get(0),
        ) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("[存储] 活跃会话计数查询失败（返回 0 降级）：{e}");
                0
            }
        }
    }

    /// 今日（本机时区零点起）token 合计与调用次数（本地 API /v1/status 用便捷
    /// 包装；与岛胶囊「今日」同口径：内部转调 today_usage(day_start)）
    pub fn today_usage_now(&self) -> (i64, i64) {
        let conn = self.lock_conn();
        let Some(cutoff) = today_start_ms(&conn) else {
            return (0, 0);
        };
        drop(conn);
        self.today_usage(cutoff)
    }

}
