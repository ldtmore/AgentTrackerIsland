//! 自定义 OpenAI 兼容·中转站适配器：one-api 系 billing 兼容接口（M3-10，
//! Balance/USD 口径）。端点与响应结构来自 one-api 源码逐段核对（2026-09-25，
//! controller/billing.go＋controller/channel-billing.go＋router/dashboard.go）：
//!   GET {base}/v1/dashboard/billing/subscription → hard_limit_usd（剩余＋已用总额）
//!   GET {base}/v1/dashboard/billing/usage        → total_usage（额度 ×100，单位 0.01 美元）
//!   净余额（美元）＝ hard_limit_usd − total_usage / 100
//! 口径陷阱（06 §10 定调"换算口径随站点配置"）：站点未开启货币显示时字段名
//! 仍叫 usd 但装的是内部点数（quota 原值），站点配置不可见、无法探测区分，
//! 照实展示＋cred_hint ⓘ 说明。base 必填（站点各异，注册表无默认端点）。

use crate::provider::{http_client, ProviderAdapter, QuotaSnapshot};
use crate::store::BalanceRow;

/// 自定义中转站无默认端点（base 必填，注册表与 create 命令双重校验）
pub const BASE_CUSTOM_OPENAI: &str = "";

pub struct CustomOpenaiProvider {
    /// 实例归属（快照入库 account_id）
    account_id: String,
    /// 中转站站点根（如 https://relay.example.com，必填）
    base: String,
    /// 中转站 API Key（Bearer）
    key: String,
    /// 复用的 blocking 客户端：共享连接池与 TLS 会话，自带 5s 超时
    http: reqwest::blocking::Client,
}

impl CustomOpenaiProvider {
    pub fn new(account_id: &str, base: &str, key: &str) -> anyhow::Result<Self> {
        Ok(Self {
            account_id: account_id.to_string(),
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            http: http_client("自定义中转")?,
        })
    }

    /// 单端点 GET（subscription/usage 共用）：HTTP 层与业务体双层拦截——
    /// one-api 的业务错误也是 HTTP 200＋响应体 {"error":{...}}，必须查体
    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str, label: &str) -> anyhow::Result<T> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.key)
            .header("Accept", "application/json")
            .send()?;
        if !resp.status().is_success() {
            anyhow::bail!("中转站 {label} 接口 HTTP {}（请检查站点地址）", resp.status());
        }
        let body: ErrorEnvelope<T> = resp.json()?;
        match body.error {
            // HTTP 200 但体带 error＝one-api 业务失败（如令牌无效），拦截不得当成功
            Some(e) => anyhow::bail!(
                "中转站 {label} 业务错误：{}",
                e.message.unwrap_or_else(|| "（站点未返回错误详情）".into())
            ),
            None => Ok(body.data),
        }
    }
}

impl ProviderAdapter for CustomOpenaiProvider {
    fn id(&self) -> &'static str {
        "custom-openai"
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    fn fetch(&self) -> anyhow::Result<QuotaSnapshot> {
        // 失败必须留痕（审查 1.1 惯例）：此处降级为"显示最近快照"，不允许无痕降级
        let result = (|| -> anyhow::Result<BalanceRow> {
            let sub: SubscriptionBody = self
                .get_json("/v1/dashboard/billing/subscription", "额度总额")?;
            let usage: UsageBody = self.get_json("/v1/dashboard/billing/usage", "已用额度")?;
            parse_balance(&sub, &usage, &self.account_id)
        })();
        match result {
            Ok(row) => {
                log::debug!("自定义中转余额查询成功：{:.2} {}", row.total, row.currency);
                Ok(QuotaSnapshot::Balance(row))
            }
            Err(e) => {
                log::warn!("自定义中转余额查询失败（降级为最近快照）：{e}");
                Err(e)
            }
        }
    }
}

// ---------- 响应解析（结构来自 one-api 源码，2026-09-25 核对） ----------

/// 错误信封：one-api 成功时顶层是 billing 字段、失败时顶层是 error——
/// 用扁平结构一次反序列化两形态（T 字段在 error 存在时缺省为 None）
#[derive(serde::Deserialize)]
struct ErrorEnvelope<T> {
    #[serde(default)]
    error: Option<ApiError>,
    #[serde(flatten)]
    data: T,
}

#[derive(serde::Deserialize)]
struct ApiError {
    /// 业务错误详情（站点自定义文案，可能中文）
    #[serde(default)]
    message: Option<String>,
}

/// billing/subscription 响应（one-api OpenAISubscriptionResponse 子集：
/// soft/system 三值恒同取 hard 即可；has_payment_method/access_until 留档不解析）
#[derive(serde::Deserialize)]
struct SubscriptionBody {
    /// 剩余＋已用总额（站点开启货币显示时为美元，未开启时为内部点数——口径随站点）
    #[serde(default)]
    hard_limit_usd: Option<f64>,
}

/// billing/usage 响应（one-api OpenAIUsageResponse 子集；daily_costs 留档不解析）
#[derive(serde::Deserialize)]
struct UsageBody {
    /// 已用额度 ×100（单位 0.01 美元，随站点货币显示配置同上）
    #[serde(default)]
    total_usage: Option<f64>,
}

/// 解析双端点响应 → 余额快照（归属传入实例）。字段缺失必须报错（站点配置
/// 千差万别，宁缺毋滥不冒充 0）；one-api 无赠金拆分与独立可用额概念，
/// granted/available 统一 None（06 §2.2 口径）
fn parse_balance(
    sub: &SubscriptionBody,
    usage: &UsageBody,
    account_id: &str,
) -> anyhow::Result<BalanceRow> {
    let hard = sub
        .hard_limit_usd
        .ok_or_else(|| anyhow::anyhow!("中转站响应缺少 hard_limit_usd"))?;
    let used = usage
        .total_usage
        .ok_or_else(|| anyhow::anyhow!("中转站响应缺少 total_usage"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(BalanceRow {
        account_id: account_id.into(),
        currency: "USD".into(),
        total: hard - used / 100.0,
        granted: None,
        available: None,
        fetched_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// one-api GetSubscription 输出样例（DisplayInCurrencyEnabled 开启时，
    /// quota/QuotaPerUnit 后的美元值；源码构造）
    const SUB_RESP: &str = r#"{
      "object": "billing_subscription",
      "has_payment_method": true,
      "soft_limit_usd": 20.5,
      "hard_limit_usd": 20.5,
      "system_hard_limit_usd": 20.5,
      "access_until": 0
    }"#;

    /// one-api GetUsage 输出样例（total_usage = 已用额度 ×100）
    const USAGE_RESP: &str = r#"{
      "object": "list",
      "total_usage": 1234.5
    }"#;

    #[test]
    fn test_parse_balance_conversion() {
        let sub: SubscriptionBody = serde_json::from_str(SUB_RESP).unwrap();
        let usage: UsageBody = serde_json::from_str(USAGE_RESP).unwrap();
        let row = parse_balance(&sub, &usage, "acc-1").unwrap();
        assert_eq!(row.account_id, "acc-1", "快照必须归属传入实例");
        assert_eq!(row.currency, "USD", "字段名字面 USD");
        assert!(
            (row.total - (20.5 - 12.345)).abs() < 1e-9,
            "净余额＝hard_limit_usd − total_usage/100"
        );
        assert_eq!(row.granted, None, "one-api 无赠金拆分，统一 None");
        assert_eq!(row.available, None, "无独立可用额概念，统一 None");
    }

    #[test]
    fn test_negative_balance_allowed() {
        // 站点数据异常（已用大于总额）时照实展示负数，不猜口径
        let sub: SubscriptionBody =
            serde_json::from_str(r#"{"hard_limit_usd": 1.0}"#).unwrap();
        let usage: UsageBody = serde_json::from_str(r#"{"total_usage": 500.0}"#).unwrap();
        let row = parse_balance(&sub, &usage, "acc-1").unwrap();
        assert!((row.total + 4.0).abs() < 1e-9);
    }

    #[test]
    fn test_missing_field_errors() {
        let sub: SubscriptionBody = serde_json::from_str(r#"{"object": "billing_subscription"}"#).unwrap();
        let usage: UsageBody = serde_json::from_str(USAGE_RESP).unwrap();
        assert!(parse_balance(&sub, &usage, "acc-1").is_err(), "缺 hard_limit_usd 必须报错");
        let sub: SubscriptionBody = serde_json::from_str(SUB_RESP).unwrap();
        let usage: UsageBody = serde_json::from_str(r#"{"object": "list"}"#).unwrap();
        assert!(parse_balance(&sub, &usage, "acc-1").is_err(), "缺 total_usage 必须报错");
    }

    /// error 信封反序列化（HTTP 200＋体 error＝业务失败）：flatten 结构下
    /// error 存在时 T 字段缺省不报错，由 get_json 查 error 拦截
    #[test]
    fn test_error_envelope_detected() {
        let env: Result<ErrorEnvelope<SubscriptionBody>, _> = serde_json::from_str(
            r#"{"error": {"message": "无效的令牌", "type": "one_api_error"}}"#,
        );
        let env = env.unwrap();
        assert!(env.error.is_some(), "error 体必须可识别");
        assert_eq!(env.error.unwrap().message.as_deref(), Some("无效的令牌"));
    }

    /// 集成：真实调用中转站（手动：设 CUSTOM_OPENAI_BASE/CUSTOM_OPENAI_KEY 后
    /// cargo test -- --ignored）
    #[test]
    #[ignore]
    fn test_real_custom_openai_fetch() {
        let (Ok(base), Ok(key)) = (
            std::env::var("CUSTOM_OPENAI_BASE"),
            std::env::var("CUSTOM_OPENAI_KEY"),
        ) else {
            println!("未设置 CUSTOM_OPENAI_BASE/CUSTOM_OPENAI_KEY，跳过真实调用");
            return;
        };
        let p = CustomOpenaiProvider::new("acc-real", &base, &key).unwrap();
        match p.fetch().unwrap() {
            QuotaSnapshot::Balance(row) => {
                println!("余额 {:.2} {}", row.total, row.currency);
                assert!(row.total.is_finite(), "余额应为有限数");
                assert_eq!(row.account_id, "acc-real");
            }
            _ => panic!("自定义中转应返回余额口径快照"),
        }
    }
}
