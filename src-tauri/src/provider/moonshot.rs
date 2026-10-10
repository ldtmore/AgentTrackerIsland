//! Moonshot（Kimi）开放平台适配器：查询货币余额（M3-3，Balance 口径）。
//! 接口与响应格式来自官方文档（platform.kimi.com/docs/api/balance，
//! 2026-09-24 核验）：
//!   GET {base}/v1/users/me/balance   Header: Authorization: Bearer <key>
//!   金额单位为人民币元（数字类型）；cash_balance 为负表示欠费；
//!   国内外站 key 完全独立（混用 401），国际站经实例 base 覆盖建独立实例。

use crate::provider::{http_client, ProviderAdapter, QuotaSnapshot};
use crate::store::BalanceRow;

/// Moonshot 国内站端点（国际站 platform.kimi.ai 的 API 域名，2026-10-09
/// 调研确证；双站 key 互不通用，混用 401）
pub const BASE_MOONSHOT: &str = "https://api.moonshot.cn";
/// Moonshot 国际站端点（变体机制 2026-10-10 入注册表；余额币种 USD）
pub const BASE_MOONSHOT_INTL: &str = "https://api.moonshot.ai";

pub struct MoonshotProvider {
    /// 实例归属（快照入库 account_id）
    account_id: String,
    /// API origin（如 https://api.moonshot.cn）
    base: String,
    /// Moonshot 平台 API key（Bearer）
    key: String,
    /// 余额币种（双站变体：国内 CNY／国际 USD，由 build_adapter 按变体传入）
    currency: &'static str,
    /// 复用的 blocking 客户端（审查 2.2.5）：共享连接池与 TLS 会话，自带 5s 超时
    http: reqwest::blocking::Client,
}

impl MoonshotProvider {
    pub fn new(
        account_id: &str,
        base: &str,
        key: &str,
        currency: &'static str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            account_id: account_id.to_string(),
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            currency,
            http: http_client("Moonshot")?,
        })
    }
}

impl ProviderAdapter for MoonshotProvider {
    fn id(&self) -> &'static str {
        "moonshot"
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    fn fetch(&self) -> anyhow::Result<QuotaSnapshot> {
        let url = format!("{}/v1/users/me/balance", self.base);
        // 失败必须留痕（审查 1.1）：此处降级为"显示最近快照"，但不允许无痕降级
        let result = (|| -> anyhow::Result<BalanceRow> {
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.key)
                .header("Accept", "application/json")
                .send()?;
            if !resp.status().is_success() {
                anyhow::bail!("Moonshot 余额接口 HTTP {}（注意：国内外站 key 混用会 401）", resp.status());
            }
            let body: BalanceResponse = resp.json()?;
            parse_balance(&body, &self.account_id, self.currency)
        })();
        match result {
            Ok(row) => {
                log::debug!("Moonshot 余额查询成功：{:.2} {}", row.total, row.currency);
                Ok(QuotaSnapshot::Balance(row))
            }
            Err(e) => {
                log::warn!("Moonshot 余额查询失败（降级为最近快照）：{e}");
                Err(e)
            }
        }
    }
}

// ---------- 响应解析（结构来自官方文档样例） ----------

#[derive(serde::Deserialize)]
struct BalanceResponse {
    /// 业务码（0 = 成功；HTTP 200 但业务失败时据此拦截，缺失时不拦由 data 兜底）
    #[serde(default)]
    code: Option<i64>,
    /// 请求状态标志（与 code 重复，留档）
    #[serde(default)]
    #[allow(dead_code)]
    status: bool,
    data: Option<BalanceData>,
}

#[derive(serde::Deserialize)]
struct BalanceData {
    /// 可用余额（现金与代金券之和），元；≤0 无法调用推理 API
    #[serde(default)]
    available_balance: Option<f64>,
    /// 代金券余额（赠金性质），元，不为负
    #[serde(default)]
    voucher_balance: Option<f64>,
    /// 现金余额，元；负值 = 欠费（此时可用余额等于代金券余额）。BalanceRow
    /// 无"现金"列：欠费可由 total - granted 反推为负，此处仅留档不单独入库
    #[serde(default)]
    #[allow(dead_code)]
    cash_balance: Option<f64>,
}

/// 解析响应 → 余额快照（归属传入实例）。available 统一填 None（06 §2.2 回写）：
/// 可用余额即 total（现金＋代金券之和），无独立于总额的可用额概念。
/// 币种由调用方按站点变体传入（国内 CNY／国际 USD），不在此硬编码
fn parse_balance(
    body: &BalanceResponse,
    account_id: &str,
    currency: &'static str,
) -> anyhow::Result<BalanceRow> {
    if let Some(c) = body.code {
        if c != 0 {
            anyhow::bail!("Moonshot 余额响应业务码异常：{c}");
        }
    }
    let data = body
        .data
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Moonshot 余额响应缺少 data"))?;
    let total = data
        .available_balance
        .ok_or_else(|| anyhow::anyhow!("Moonshot 余额响应缺少 available_balance"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(BalanceRow {
        account_id: account_id.into(),
        currency: currency.into(),
        total,
        granted: data.voucher_balance,
        available: None,
        fetched_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方文档响应样例（platform.kimi.com/docs/api/balance）
    const DOC_RESP: &str = r#"{
        "code": 0,
        "data": {
            "available_balance": 49.58894,
            "voucher_balance": 46.58893,
            "cash_balance": 3.00001
        },
        "scode": "0x0",
        "status": true
    }"#;

    #[test]
    fn test_parse_doc_response() {
        let body: BalanceResponse = serde_json::from_str(DOC_RESP).unwrap();
        let row = parse_balance(&body, "acc-1", "CNY").unwrap();
        assert_eq!(row.account_id, "acc-1", "快照必须归属传入实例");
        assert_eq!(row.currency, "CNY");
        assert!((row.total - 49.58894).abs() < 1e-9, "total 应取 available_balance");
        assert!((row.granted.unwrap() - 46.58893).abs() < 1e-9, "granted 应取 voucher_balance");
        assert_eq!(row.available, None, "无独立可用额概念，统一 None");
    }

    #[test]
    fn test_business_code_and_missing_data() {
        // HTTP 200 但业务码非 0：必须拦截，不得把空数据当成功
        let body: BalanceResponse =
            serde_json::from_str(r#"{"code": 1002, "status": false}"#).unwrap();
        assert!(parse_balance(&body, "acc-1", "CNY").is_err(), "业务码非 0 应报错");
        // 业务码为 0 但缺 data：data 校验兜底拦截
        let body: BalanceResponse = serde_json::from_str(r#"{"code": 0}"#).unwrap();
        assert!(parse_balance(&body, "acc-1", "CNY").is_err(), "缺少 data 应报错");
    }

    #[test]
    fn test_negative_cash_is_tolerated() {
        // 欠费场景：cash_balance 为负拉低总额，f64 天然承载负数
        let body: BalanceResponse = serde_json::from_str(
            r#"{"code": 0, "data": {"available_balance": 1.5, "voucher_balance": 10.0, "cash_balance": -8.5}}"#,
        )
        .unwrap();
        let row = parse_balance(&body, "acc-1", "CNY").unwrap();
        assert_eq!(row.total, 1.5, "欠费时 total 取实际可用余额");
        assert_eq!(row.granted, Some(10.0), "total - granted 为负即现金欠费");
    }

    #[test]
    fn test_intl_variant_currency_is_usd() {
        // 双站变体（2026-10-10）：国际站余额币种 USD 由调用方传入，解析层不硬编码
        let body: BalanceResponse = serde_json::from_str(DOC_RESP).unwrap();
        let row = parse_balance(&body, "acc-intl", "USD").unwrap();
        assert_eq!(row.currency, "USD");
    }

    /// 集成：真实调用余额接口（手动：设 MOONSHOT_API_KEY 后 cargo test -- --ignored）
    #[test]
    #[ignore]
    fn test_real_moonshot_fetch() {
        let Ok(key) = std::env::var("MOONSHOT_API_KEY") else {
            println!("未设置 MOONSHOT_API_KEY，跳过真实调用");
            return;
        };
        let p = MoonshotProvider::new("acc-real", BASE_MOONSHOT, &key, "CNY").unwrap();
        match p.fetch().unwrap() {
            QuotaSnapshot::Balance(row) => {
                println!("余额 {:.2} {}（赠金 {:?}）", row.total, row.currency, row.granted);
                assert!(row.total.is_finite(), "余额应为有限数");
                assert_eq!(row.account_id, "acc-real");
            }
            _ => panic!("Moonshot 应返回余额口径快照"),
        }
    }
}
