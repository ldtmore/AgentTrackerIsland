-- AgentTrackerIsland 迁移 0008（2026-09-29，全面审查修复 #4）
-- 常驻查询索引补齐：三张快照/事件表的索引设计与高频查询路径错配，
-- 数据量增长后出现随时间线性劣化的全表扫描（详见各条目注释）。

-- ① status_events 此前零索引：它是增长最快的表之一（hooks 每工具调用 1~2 条），
--    session_detail 的 COUNT/列表（WHERE session_id=?1 OR session_id=?2）与
--    cleanup 的按 ts 删除全为全表扫描
CREATE INDEX IF NOT EXISTS idx_status_session ON status_events(session_id, ts);

-- ② 额度历史曲线（quota_history：WHERE fetched_at>=? ORDER BY fetched_at）与
--    按 fetched_at 的 cleanup：原索引前导列是 account_id 不命中，全表扫＋排序
CREATE INDEX IF NOT EXISTS idx_quota_fetched ON quota_snapshots(fetched_at);
CREATE INDEX IF NOT EXISTS idx_balance_fetched ON balance_snapshots(fetched_at);

-- ③ latest_quotas 窗口函数支撑：分区键 (provider, window_kind, IFNULL(account_id,''))
--    的表达式索引（SQLite 确定性函数可作索引列），使 PARTITION BY ... ORDER BY id
--    可按索引序流式处理，免全表物化排序。原相关子查询对此外层每行执行一次，
--    且 IFNULL 包裹列使 idx_quota_account 完全失效——每 tick（1~10s）近全表扫
CREATE INDEX IF NOT EXISTS idx_quota_latest
  ON quota_snapshots(provider, window_kind, IFNULL(account_id,''), id);
