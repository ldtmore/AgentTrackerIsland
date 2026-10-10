-- AgentTrackerIsland 迁移 0011（2026-10-03，设置页卡片拖拽排序）
-- provider_accounts 加用户自定义排序列（list_provider_accounts 的 ORDER BY 首键）；
-- 存量行按「创建序」回填行号——升级用户的初始顺序与迁移前完全一致（零感知），
-- 之后才被用户拖拽改写。不建索引：实例为个位数规模，排序键走全表扫描足够（YAGNI）
ALTER TABLE provider_accounts ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;
UPDATE provider_accounts
SET sort_order = (
  SELECT COUNT(*)
  FROM provider_accounts p2
  WHERE p2.created_at < provider_accounts.created_at
     OR (p2.created_at = provider_accounts.created_at AND p2.id < provider_accounts.id)
);
