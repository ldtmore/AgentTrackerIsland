-- 0010（2026-10-03 审查优化）：会话明细三连查询消排序。
-- idx_usage_session 原为单列（session_id），「定位会话→按 ts 排序」路径
-- （会话详情抽屉调用流水 / latest_session_errors / latest_session_models）
-- 每次都要建 TEMP B-TREE 排序（真实库 EXPLAIN 实证）；升级为 (session_id, ts)
-- 复合索引后三条常驻查询免排序，全部现有查询计划自动受益，零代码改动。
-- 写入侧代价可忽略（10s 批量写入本就维护该索引，仅多带一列）。
DROP INDEX IF EXISTS idx_usage_session;
CREATE INDEX idx_usage_session ON usage_records(session_id, ts);
