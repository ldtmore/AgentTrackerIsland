-- AgentTrackerIsland 迁移 0006（2026-09-24，M3-2 供应商实例层）
-- 供应商实例：用户录入的"某厂商的一个账号"（06-PLAN §3.1 / D3 / D14）；
-- 同厂商可多条实例（多 Key 即多实例），别名同厂商内唯一。
-- 凭据双模式：plain 的 key 真值存系统钥匙串（SQLite 不落盘），env 只存变量名。

CREATE TABLE IF NOT EXISTS provider_accounts (
  id            TEXT PRIMARY KEY,               -- uuid v4
  kind_id       TEXT NOT NULL,                  -- 厂商 id（ProviderKindDef.id）
  alias         TEXT NOT NULL,                  -- 实例别名（显示名）
  base_override TEXT,                           -- 覆盖厂商默认端点（NULL=用默认）
  cred_kind     TEXT NOT NULL DEFAULT 'plain',  -- 'plain'（钥匙串）| 'env'（环境变量引用）
  cred_value    TEXT,                           -- env：变量名；plain：NULL（真值在钥匙串）
  note          TEXT,                           -- 备注（discovered 实例的来源留痕，06-PLAN §3.3）
  enabled       INTEGER NOT NULL DEFAULT 1,
  in_island     INTEGER NOT NULL DEFAULT 1,     -- 岛展示集多选（D7，M3-6 消费）
  origin        TEXT NOT NULL DEFAULT 'manual', -- manual | discovered
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  UNIQUE(kind_id, alias)
);
