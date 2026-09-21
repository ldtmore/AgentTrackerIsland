-- AgentTrackerIsland 迁移 0004(2026-09-21,0003 落地修复)
-- 背景:0003 的首版 SQL 用 CREATE TABLE IF NOT EXISTS——在已有旧表的库上被静默跳过,
-- 但 user_version 仍被写到 3,导致修复版 0003(DROP+CREATE)永远不再执行:
-- 表停留在旧结构(无 source_id/is_background),新代码写入全部失败或旧逻辑持续双计。
-- 0004 无条件重建为 0003 同款结构,对任何中间状态库一次收敛;
-- 已是 15 列的正常库会多经历一次清空重建(全量回溯自动恢复,幂等无害)。

DROP TABLE IF EXISTS usage_records;

CREATE TABLE usage_records (
  id                       INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id               TEXT NOT NULL,
  agent                    TEXT NOT NULL,
  model                    TEXT NOT NULL,
  provider                 TEXT,
  ts                       INTEGER NOT NULL,
  input_tokens             INTEGER,
  output_tokens            INTEGER,
  reasoning_tokens         INTEGER,
  cache_read_tokens        INTEGER,
  cache_creation_tokens    INTEGER,
  duration_ms              INTEGER,
  ttft_ms                  INTEGER,
  error_type               TEXT,
  -- 真实消息身份:同源多行(流式复制快照)靠它识别,upsert 保留最大快照
  source_id                TEXT,
  -- 后台用量校准行(cost-state 差值):计入 token,不计入调用次数
  is_background            INTEGER NOT NULL DEFAULT 0,
  UNIQUE(agent, session_id, source_id)
);

CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage_records(ts);
CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_records(session_id);

-- 重建标志:聚合器首轮读到后清空水位,采集层全量回溯重建
INSERT OR REPLACE INTO app_settings(key, value) VALUES('usage_rebuild_pending', '1');
