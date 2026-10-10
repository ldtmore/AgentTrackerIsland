//! 供应商实例装载与 GLM 存量凭据一次性迁移（M3-2，06-PLAN §3.3/§4.3）。
//! 启动时执行：
//!   ① run_glm_migration：应用设置/suppliers.json 的旧凭据 → 实例＋钥匙串（防重入）；
//!   ② discover_missing：零实例厂商跑发现器补建 discovered 实例（发现器只当建议者）；
//!   ③ 装载：实例表 → 凭据解析 → 工厂 → 适配器列表（解析失败分类留痕跳过）。

use crate::provider::{
    build_adapter, kind_of, resolve_creds, AccountCreds, CredState, DiscoveredCreds,
    ProviderAdapter, ProviderDiscoverer, KEYRING_SERVICE,
};
use crate::provider::glm::{GlmDiscoverer, BASE_BIGMODEL};
use crate::store::{ProviderAccount, Store};
use std::sync::Arc;

/// 迁移防重入标志（app_settings 键；写"1"后不再执行存量迁移）
pub const MIGRATED_KEY: &str = "provider_migrated_v1";

/// GLM 迁移决策（06-PLAN §3.3 三分支；决策与副作用分离，纯函数可测）
#[derive(Debug, PartialEq)]
pub enum GlmMigrateDecision {
    /// 应用设置显式配置（glm_token 非空）：真值转钥匙串，base≠默认时写覆盖
    FromSettings { base: String, token: String },
    /// 自动发现命中（env/suppliers.json）：origin=discovered，来源写入 note
    Discovered(DiscoveredCreds),
    /// 均无：不建实例，设置页显示引导空态
    None,
}

/// 迁移决策（纯函数）：应用设置显式配置优先于自动发现（与旧聚合器初始化的
/// 优先级一致）；token 为空串视为无配置（兼容"清空即回落自动"的旧语义）
pub fn glm_migrate_decision(
    glm_base: Option<&str>,
    glm_token: Option<&str>,
    discovered: Option<DiscoveredCreds>,
) -> GlmMigrateDecision {
    if let Some(tok) = glm_token.map(str::trim).filter(|t| !t.is_empty()) {
        let base = glm_base
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| BASE_BIGMODEL.to_string());
        return GlmMigrateDecision::FromSettings { base, token: tok.to_string() };
    }
    match discovered {
        Some(c) => GlmMigrateDecision::Discovered(c),
        None => GlmMigrateDecision::None,
    }
}

/// base 覆盖判断：与厂商 kind 级默认端点一致则不存（NULL = 用默认）。注意
/// 只有 kind.default_base（默认变体/国内站）可省略——resolve_creds 的空覆盖
/// 语义是回落 kind.default_base，国际站端点必须显式存覆盖，否则请求会静默
/// 落到国内站（2026-10-10 双站变体机制下的关键不变量）
fn base_override_for(kind_id: &str, base: &str) -> Option<String> {
    let b = base.trim_end_matches('/').to_string();
    let is_default = crate::provider::kind_of(kind_id)
        .map(|k| k.default_base == b)
        .unwrap_or(false);
    if is_default { None } else { Some(b) }
}

/// 建实例并把明文 key 真值转存钥匙串（D11：写入失败报错中止，不静默明文落盘）。
/// 迁移与设置页创建共用底层（M3-4）：错误透传，由调用方按各自策略处理
/// （迁移＝吞错重试；设置页＝报错引导改 env 模式）
pub fn insert_account_with_keyring(
    store: &Store,
    kind_id: &str,
    alias: &str,
    base_override: Option<String>,
    origin: &str,
    note: Option<&str>,
    key: &str,
) -> anyhow::Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();
    let account = ProviderAccount {
        id: id.clone(),
        kind_id: kind_id.to_string(),
        alias: alias.to_string(),
        base_override,
        cred_kind: "plain".to_string(),
        cred_value: None, // 真值在钥匙串，SQLite 不落盘
        note: note.map(str::to_string),
        enabled: true,
        in_island: true,
        origin: origin.to_string(),
        created_at: now,
        updated_at: now,
    };
    // 先写钥匙串再插库：钥匙串失败 → 中止（无遗留行）；插库失败（极罕见）→
    // 中止重试，遗留孤儿钥匙串条目无副作用
    keyring::Entry::new(KEYRING_SERVICE, &id)
        .and_then(|e| e.set_password(key))
        .map_err(|e| anyhow::anyhow!("凭据写入钥匙串失败：{e}"))?;
    store.insert_provider_account(&account)?;
    log::info!("[实例] 已创建供应商实例：{alias}（origin={origin}，id={id}）");
    Ok(id)
}

/// 建实例（迁移路径薄封装）：吞错误返回 None——迁移中止不写防重入标志，
/// 下次启动自动重试（错误细节已在底层留痕）
fn create_instance(
    store: &Store,
    kind_id: &str,
    alias: &str,
    base_override: Option<String>,
    origin: &str,
    note: Option<&str>,
    key: &str,
) -> Option<String> {
    match insert_account_with_keyring(store, kind_id, alias, base_override, origin, note, key) {
        Ok(id) => Some(id),
        Err(e) => {
            log::error!(
                "[迁移] 实例 {alias} 创建失败，迁移中止（该厂商额度降级为最近快照；下次启动自动重试）：{e:#}"
            );
            None
        }
    }
}

/// GLM 存量一次性迁移：决策 → 建实例 → 回填存量快照归属 → 写防重入标志
fn run_glm_migration(store: &Store) {
    if store.get_setting(MIGRATED_KEY).as_deref() == Some("1") {
        return;
    }
    let discovered = GlmDiscoverer.discover().into_iter().next();
    let decision = glm_migrate_decision(
        store.get_setting("glm_base").as_deref(),
        store.get_setting("glm_token").as_deref(),
        discovered,
    );
    let migrated_account: Option<String> = match decision {
        GlmMigrateDecision::FromSettings { base, token } => {
            // 旧键保留不再读取（兼容回滚）；来源延续旧 glm_token_source 的"应用设置"口径
            let created = create_instance(
                store, "glm", "GLM", base_override_for("glm", &base), "manual", Some("应用设置"), &token,
            );
            if let Some(account_id) = &created {
                // 存量 GLM 快照全部划归迁移实例（未回填行仅作历史展示）
                let n = store.backfill_quota_account("glm", account_id);
                if n > 0 {
                    log::info!("[迁移] 存量 GLM 快照 {n} 条已回填实例归属");
                }
            }
            created
        }
        GlmMigrateDecision::Discovered(c) => {
            let alias = discovered_alias("glm");
            let created = create_instance(
                store, "glm", &alias, base_override_for("glm", &c.base), "discovered", Some(c.source), &c.key,
            );
            if let Some(account_id) = &created {
                let n = store.backfill_quota_account("glm", account_id);
                if n > 0 {
                    log::info!("[迁移] 存量 GLM 快照 {n} 条已回填实例归属");
                }
            }
            created
        }
        // 无凭据：不写防重入标志——用户随后在设置页配置凭据时，下次启动仍能
        // 迁移为实例（无副作用的重试，开销可忽略）
        GlmMigrateDecision::None => None,
    };
    if migrated_account.is_some() {
        store.set_setting(MIGRATED_KEY, "1");
        log::info!("[迁移] GLM 存量凭据迁移完成（防重入标志已写入）");
    }
}

/// discovered 实例别名："{厂商显示名}（自动发现）"（06-PLAN §4.3）
fn discovered_alias(kind_id: &str) -> String {
    format!(
        "{}（自动发现）",
        kind_of(kind_id).map(|k| k.name).unwrap_or(kind_id)
    )
}

/// 零实例厂商跑发现器补建（§4.3）：已有实例的厂商跳过；发现器只当建议者，
/// 一切以实例表为准。钥匙串写入失败的厂商跳过（下次启动重试），不阻塞装载
fn discover_missing(store: &Store) {
    // 发现器注册表：第一批仅 GLM 迁移完毕；其余厂商凭据发现列后续池
    let discoverers: Vec<Box<dyn ProviderDiscoverer>> = vec![Box::new(GlmDiscoverer)];
    for d in discoverers {
        if store.list_provider_accounts().iter().any(|a| a.kind_id == d.kind_id()) {
            continue;
        }
        let Some(c) = d.discover().into_iter().next() else {
            log::debug!("[发现] {} 未发现凭据（设置页可手动配置）", d.kind_id());
            continue;
        };
        let alias = discovered_alias(d.kind_id());
        create_instance(store, d.kind_id(), &alias, base_override_for(d.kind_id(), &c.base), "discovered", Some(c.source), &c.key);
    }
}

/// 启动装载入口：迁移 → 发现补建 → 实例表转适配器列表。
/// 凭据解析失败的实例按分类留痕跳过（不阻塞其他实例；其额度自然降级为最近快照）
pub fn load_instances(store: Arc<Store>) -> Vec<Box<dyn ProviderAdapter>> {
    run_glm_migration(&store);
    discover_missing(&store);
    let mut out = vec![];
    for a in store.list_provider_accounts() {
        if !a.enabled {
            continue; // 停用实例不装载（不删除）
        }
        let creds: Result<AccountCreds, CredState> = resolve_creds(&a);
        match creds {
            Ok(creds) => match build_adapter(&a, &creds, store.clone()) {
                Ok(adapter) => out.push(adapter),
                Err(e) => log::warn!("[装载] 实例 {}（{}）适配器构建失败，跳过：{e}", a.alias, a.id),
            },
            Err(state) => {
                log::warn!(
                    "[装载] 实例 {}（{}）凭据不可用——{}（额度显示最近快照，修复后重启生效）",
                    a.alias,
                    a.id,
                    state.describe()
                );
            }
        }
    }
    out
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 决策三分支：应用设置显式配置优先于自动发现
    #[test]
    fn test_decision_prefers_settings() {
        let disc = || {
            Some(DiscoveredCreds {
                base: "https://api.z.ai".into(),
                key: "discovered-key".into(),
                source: "环境变量",
            })
        };
        // settings 有 token：走 FromSettings，discover 结果被忽略
        let d = glm_migrate_decision(Some("https://api.z.ai"), Some("sk-manual"), disc());
        assert_eq!(
            d,
            GlmMigrateDecision::FromSettings {
                base: "https://api.z.ai".into(),
                token: "sk-manual".into()
            }
        );
        // settings 无值：回落发现结果
        let d = glm_migrate_decision(None, None, disc());
        match d {
            GlmMigrateDecision::Discovered(c) => assert_eq!(c.source, "环境变量"),
            other => panic!("应为 Discovered：{other:?}"),
        }
        // 均无：不建实例
        assert_eq!(glm_migrate_decision(None, None, None), GlmMigrateDecision::None);
    }

    /// 决策边界：空串 token/空串 base 视为无配置（兼容"清空即回落自动"旧语义）
    #[test]
    fn test_decision_blank_fallback() {
        let disc = Some(DiscoveredCreds {
            base: BASE_BIGMODEL.into(),
            key: "k".into(),
            source: "claude-menu 配置",
        });
        let d = glm_migrate_decision(Some("  "), Some("  "), disc);
        match d {
            GlmMigrateDecision::Discovered(_) => {}
            other => panic!("空白值应回落发现链：{other:?}"),
        }
        // token 有值而 base 缺省：base 用默认端点
        let d = glm_migrate_decision(None, Some("sk"), None);
        assert_eq!(
            d,
            GlmMigrateDecision::FromSettings { base: BASE_BIGMODEL.into(), token: "sk".into() }
        );
    }

    /// base 覆盖判断：仅 kind 级默认端点不存覆盖；国际站端点必须显式存覆盖
    /// （resolve_creds 空覆盖回落国内站，省略会让 z.ai 实例静默请求
    /// bigmodel.cn——双站变体机制的关键不变量）；非标地址归一尾斜杠
    #[test]
    fn test_base_override() {
        assert_eq!(base_override_for("glm", BASE_BIGMODEL), None);
        assert_eq!(base_override_for("glm", "https://open.bigmodel.cn/"), None);
        // 国际站端点必须显式存覆盖（省略 = 静默回落国内站）
        assert_eq!(
            base_override_for("glm", "https://api.z.ai"),
            Some("https://api.z.ai".to_string())
        );
        assert_eq!(
            base_override_for("glm", "https://api.z.ai/"),
            Some("https://api.z.ai".to_string())
        );
        // 非标地址（自建反代等）仍存覆盖
        assert_eq!(
            base_override_for("glm", "https://glm-proxy.example.com"),
            Some("https://glm-proxy.example.com".to_string())
        );
    }

    /// 别名拼接：厂商显示名 + "（自动发现）"（2026-10-10 定名后 GLM 显示名）
    #[test]
    fn test_discovered_alias() {
        assert_eq!(discovered_alias("glm"), "GLM（自动发现）");
        assert_eq!(discovered_alias("unknown"), "unknown（自动发现）");
    }
}
