-- AgentTrackerIsland 迁移 0002(2026-09-17 审查优化 2.2.2/P3)
-- ① 会话维度索引:会话级批量聚合(GROUP BY session_id / 窗口函数取最新模型)
--    在无索引时是对全表扫描;0001 的 UNIQUE(agent, session_id, ts, model) 前缀是
--    agent,按 session_id 过滤用不上,故补专属索引
CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_records(session_id);

-- ② 移除从未使用的水位偏移列(CC 增量游标实际落在内存 per-file 偏移表,见
--    collector/claude_code.rs;单值 last_offset 无法表达多文件语义)
ALTER TABLE watermarks DROP COLUMN last_offset;
