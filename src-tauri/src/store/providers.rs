// 供应商实例域：实例 CRUD/别名唯一/单价覆盖表
// （2026-10-03 审查拆分：自 mod.rs 纯移动，方法体逐字节不变；领域内聚，
// Store 方法经 inherent impl 就地扩展，外部调用路径零变化）

use super::*;

impl Store {
    /// 插入供应商实例（id 由调用方生成 uuid；别名全局唯一，冲突报错——
    /// 0009 跨厂商唯一索引＋0006 表级 UNIQUE(kind_id, alias) 双保险，后者是前者的子集）。
    /// sort_order 不由调用方指定：SQL 内取当前最大值＋1，新实例恒排列表末尾（符合直觉）
    pub fn insert_provider_account(&self, a: &ProviderAccount) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO provider_accounts(
               id, kind_id, alias, base_override, cred_kind, cred_value, note,
               enabled, in_island, origin, created_at, updated_at, sort_order)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,
               (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM provider_accounts))",
            params![
                a.id, a.kind_id, a.alias, a.base_override, a.cred_kind, a.cred_value, a.note,
                a.enabled as i64, a.in_island as i64, a.origin, a.created_at, a.updated_at
            ],
        )
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("实例插入失败（alias={}）：{e}", a.alias))
    }

    /// 全部实例（装载与后续管理 UI 共用）。顺序＝用户自定义排序（0011 拖拽排序），
    /// 二三级键兜底：未提交排序的新实例／同值行按创建序稳定输出——本顺序是
    /// 岛轮播、岛胶囊、托盘菜单、额度页、报表曲线标签、本地 API 的全局唯一顺序源
    pub fn list_provider_accounts(&self) -> Vec<ProviderAccount> {
        let conn = self.lock_conn();
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, kind_id, alias, base_override, cred_kind, cred_value, note,
                    enabled, in_island, origin, created_at, updated_at
             FROM provider_accounts ORDER BY sort_order ASC, created_at ASC, id ASC",
        ) else {
            return vec![];
        };
        let rows = stmt.query_map([], |r| {
            Ok(ProviderAccount {
                id: r.get(0)?,
                kind_id: r.get(1)?,
                alias: r.get(2)?,
                base_override: r.get(3)?,
                cred_kind: r.get(4)?,
                cred_value: r.get(5)?,
                note: r.get(6)?,
                enabled: r.get::<_, i64>(7)? != 0,
                in_island: r.get::<_, i64>(8)? != 0,
                origin: r.get(9)?,
                created_at: r.get(10)?,
                updated_at: r.get(11)?,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] list_provider_accounts 查询失败（本轮返回空降级）：{e}");
                vec![]
            },
        }
    }

    /// 启用/停用实例（停用不删除）。Err 透传（四轮审查）：原先吞错返回 ()，
    /// 前端乐观 UI 无法感知失败＝「点了没反应」；n==0（实例不存在）同样报错
    pub fn set_provider_account_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        let n = conn
            .execute(
                "UPDATE provider_accounts SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
                params![enabled as i64, now_ms(), id],
            )
            .map_err(|e| anyhow::anyhow!("实例启停写入失败（id={id}）：{e}"))?;
        if n == 0 {
            anyhow::bail!("实例不存在：{id}");
        }
        Ok(())
    }

    /// 编辑实例（M3-4）：厂商锁定不可改（kind_id 变更＝删了重建）；note 是
    /// discovered 来源留痕，编辑表单不触碰由调用方透传。别名全局唯一
    /// （0009 idx_provider_accounts_alias，2026-09-30 D14 升级），冲突 Err 透传给设置页即时红字
    #[allow(clippy::too_many_arguments)]
    pub fn update_provider_account(
        &self,
        id: &str,
        alias: &str,
        base_override: Option<String>,
        cred_kind: &str,
        cred_value: Option<String>,
        note: Option<String>,
    ) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        let n = conn
            .execute(
                "UPDATE provider_accounts
                 SET alias = ?1, base_override = ?2, cred_kind = ?3, cred_value = ?4,
                     note = ?5, updated_at = ?6
                 WHERE id = ?7",
                params![alias, base_override, cred_kind, cred_value, note, now_ms(), id],
            )
            .map_err(|e| anyhow::anyhow!("实例更新失败（alias={alias}）：{e}"))?;
        if n == 0 {
            anyhow::bail!("实例不存在：{id}");
        }
        Ok(())
    }

    /// 删除实例（M3-4）。历史快照保留（quota/balance_snapshots 按 account_id 的
    /// 孤儿行仅作历史展示，不破坏旧数据——拍板决策 2026-09-24）；钥匙串条目
    /// 由命令层删除，此处只管库
    pub fn delete_provider_account(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        let n = conn
            .execute("DELETE FROM provider_accounts WHERE id = ?1", params![id])
            .map_err(|e| anyhow::anyhow!("实例删除失败（id={id}）：{e}"))?;
        if n == 0 {
            anyhow::bail!("实例不存在：{id}");
        }
        Ok(())
    }

    /// 岛展示集切换（M3-4 多选器；软上限 5 由前端提示，库内只存用户意愿——
    /// 停用实例是否进轮播由渲染层按 enabled 判定，此处不联动改 in_island）。
    /// Err 透传（四轮审查，与 set_provider_account_enabled 同款）
    pub fn set_provider_account_in_island(&self, id: &str, in_island: bool) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        let n = conn
            .execute(
                "UPDATE provider_accounts SET in_island = ?1, updated_at = ?2 WHERE id = ?3",
                params![in_island as i64, now_ms(), id],
            )
            .map_err(|e| anyhow::anyhow!("实例岛展示集写入失败（id={id}）：{e}"))?;
        if n == 0 {
            anyhow::bail!("实例不存在：{id}");
        }
        Ok(())
    }

    /// 用户自定义排序（设置页卡片拖拽）：按传入 id 顺序整体重写 sort_order（0..n），
    /// 前端始终提交全量列表。单事务保证中途失败不留半新半旧；不存在的 id 更新
    /// 0 行自然忽略（并发删除容错）；列表之外的实例保持原值，由 list 的二三级
    /// 排序键兜底稳定。不推进 updated_at——排序是界面编排不是实例配置变更
    /// （仿 0009 清洗先例「不伪造最近更新」），也避免热刷新指纹无谓抖动
    pub fn reorder_provider_accounts(&self, ids: &[String]) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| anyhow::anyhow!("排序事务开启失败：{e}"))?;
        for (idx, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE provider_accounts SET sort_order = ?1 WHERE id = ?2",
                params![idx as i64, id],
            )
            .map_err(|e| anyhow::anyhow!("排序写入失败（id={id}）：{e}"))?;
        }
        tx.commit()
            .map_err(|e| anyhow::anyhow!("排序事务提交失败：{e}"))?;
        Ok(())
    }

    /// 用户模型单价覆盖（迁移 0005，M3-1）：行数极少（个位数），整表读入内存
    pub fn list_price_overrides(&self) -> std::collections::HashMap<String, crate::modelcat::PriceOverride> {
        let conn = self.lock_conn();
        let Ok(mut stmt) = conn.prepare(
            "SELECT model_lower, input_cost_per_token, output_cost_per_token,
                    cache_read_cost, cache_creation_cost
             FROM model_price_overrides",
        ) else {
            return Default::default();
        };
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                crate::modelcat::PriceOverride {
                    input: r.get(1)?,
                    output: r.get(2)?,
                    cache_read: r.get(3)?,
                    cache_creation: r.get(4)?,
                },
            ))
        });
        match rows {
            Ok(it) => it.filter_map(|x| x.ok()).collect(),
            Err(e) => {
                log::warn!("[存储] list_price_overrides 查询失败（本轮返回空降级）：{e}");
                Default::default()
            },
        }
    }

}
