-- AgentTrackerIsland 迁移 0005（2026-09-24，M3-1 模型目录与成本口径）
-- 用户模型单价覆盖表（06-PLAN §3.2 / D12）：单价来源优先级 = 本表 → LiteLLM 快照。
-- model 名一律小写规范化后存储；编辑 UI 列后续池，本批只建表并保证读取生效。

CREATE TABLE IF NOT EXISTS model_price_overrides (
  model_lower              TEXT PRIMARY KEY,
  input_cost_per_token     REAL NOT NULL,
  output_cost_per_token    REAL NOT NULL,
  cache_read_cost          REAL,
  cache_creation_cost      REAL,
  note                     TEXT,
  updated_at               INTEGER NOT NULL
);
