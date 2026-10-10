-- AgentTrackerIsland 迁移 0007（2026-09-24，M3-2 供应商实例层）
-- 快照表扩实例维度 + 货币余额快照表（06-PLAN §3.2）。
-- quota_snapshots 存量行的 account_id 由 GLM 存量迁移（§3.3）回填；
-- 未回填行（provider_migrated_v1 前的非 GLM 数据，理论上不存在）仅作历史展示。

ALTER TABLE quota_snapshots ADD COLUMN account_id TEXT;

-- 货币余额快照（Balance 口径，与窗口口径结构不同故独立表；余额历史曲线的数据基础）
CREATE TABLE balance_snapshots (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id  TEXT NOT NULL,
  currency    TEXT NOT NULL,
  total       REAL,
  granted     REAL,
  available   REAL,
  fetched_at  INTEGER NOT NULL
);
CREATE INDEX idx_balance_account ON balance_snapshots(account_id, fetched_at);
CREATE INDEX idx_quota_account ON quota_snapshots(account_id, fetched_at);
