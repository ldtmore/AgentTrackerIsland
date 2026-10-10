-- AgentTrackerIsland 迁移 0009（2026-09-30，D14 升级：别名全局唯一）
-- 原「同厂商内唯一」（0006 表级 UNIQUE(kind_id, alias)）升为跨厂商全局唯一——
-- 面板帧头/额度卡降噪改版后，别名需独立承载实例身份（厂商名收进悬浮/角标）。
-- 表级旧约束保留不动：它是新约束的子集，SQLite 改表级约束需重建整表，侵入大不值。
-- ⚠️ 建索引前必须无重名数据，否则 CREATE UNIQUE INDEX 失败＝迁移回滚＝应用起不来；
-- 重名清洗由 Rust 侧在同一迁移事务内完成（Store::dedup_provider_aliases，
-- 重名者按创建序保留首个、其余自动追加「·N」后缀，N 对现有别名集合查重取最小可用值）。

CREATE UNIQUE INDEX IF NOT EXISTS idx_provider_accounts_alias
  ON provider_accounts(alias);
