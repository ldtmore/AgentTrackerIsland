//! DeepSeek 开放平台适配器：查询货币余额（M3-3，Balance 口径）。
//! 接口与响应格式来自官方文档（api-docs.deepseek.com/api/get-user-balance，
//! 2026-09-24 核验）：
//!   GET {base}/user/balance   Header: Authorization: Bearer <key>
//!   balance_infos[] 各金额字段均为字符串；多币种账户可能同时含 CNY/USD 多笔。

use crate::provider::{http_client, ProviderAdapter, QuotaSnapshot};
use crate::store::BalanceRow;

/// DeepSeek 开放平台端点
pub const BASE_DEEPSEEK: &str = "https://api.deepseek.com";

pub struct DeepseekProvider {
    /// 实例归属（快照入库 account_id）
    account_id: String,
    /// API origin（如 https://api.deepseek.com）
    base: String,
    /// DeepSeek 平台 API key（Bearer）
    key: String,
    /// 复用的 blocking 客户端（审查 2.2.5）：共享连接池与 TLS 会话，自带 5s 超时
    http: reqwest::blocking::Client,
}

impl DeepseekProvider {
    pub fn new(account_id: &str, base: &str, key: &str) -> anyhow::Result<Self> {
        Ok(Self {
            account_id: account_id.to_string(),
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            http: http_client("DeepSeek")?,
        })
    }
}

impl ProviderAdapter for DeepseekProvider {
    fn id(&self) -> &'static str {
        "deepseek"
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    fn fetch(&self) -> anyhow::Result<QuotaSnapshot> {
        let url = format!("{}/user/balance", self.base);
        // 失败必须留痕（审查 1.1）：此处降级为"显示最近快照"，但不允许无痕降级
        let result = (|| -> anyhow::Result<BalanceRow> {
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.key)
                .header("Accept", "application/json")
                .send()?;
            if !resp.status().is_success() {
                anyhow::bail!("DeepSeek 余额接口 HTTP {}", resp.status());
            }
            let body: BalanceResponse = resp.json()?;
            parse_balance(&body, &self.account_id)
        })();
        match result {
            Ok(row) => {
                log::debug!("DeepSeek 余额查询成功：{:.2} {}", row.total, row.currency);
                Ok(QuotaSnapshot::Balance(row))
            }
            Err(e) => {
                log::warn!("DeepSeek 余额查询失败（降级为最近快照）：{e}");
                Err(e)
            }
        }
    }
}

// ---------- 响应解析（结构来自官方文档样例） ----------

#[derive(serde::Deserialize)]
struct BalanceResponse {
    /// 用户余额是否足够 API 调用（false = 余额不足；不影响余额入库，留档）
    #[serde(default)]
    #[allow(dead_code)]
    is_available: bool,
    #[serde(default)]
    balance_infos: Vec<CurrencyBalance>,
}

#[derive(serde::Deserialize)]
struct CurrencyBalance {
    /// 币种："CNY" | "USD"
    #[serde(default)]
    currency: String,
    /// 总余额（赠金与充值之和），字符串金额
    #[serde(default)]
    total_balance: Option<String>,
    /// 尚未过期的赠金总额，字符串金额
    #[serde(default)]
    granted_balance: Option<String>,
    /// 充值余额总额，字符串金额。BalanceRow 无"充值"列：现金部分可由
    /// total - granted 反推，此处仅留档不单独入库
    #[serde(default)]
    #[allow(dead_code)]
    topped_up_balance: Option<String>,
}

/// 字符串金额解析（官方返回如 "110.00"）；解析不出返回 None——宁缺毋滥，
/// UI 显示"--"，不拿脏值冒充金额
fn parse_amount(v: &Option<String>) -> Option<f64> {
    v.as_deref()?.trim().parse::<f64>().ok()
}

/// 解析响应 → 余额快照（归属传入实例）。多币种账户 balance_infos 可能含
/// CNY/USD 多笔：优先取 CNY（国内主币种口径），无 CNY 退回第一笔。
/// available 统一填 None（06 §2.2 回写）：DeepSeek 无独立于总额的可用额概念
fn parse_balance(body: &BalanceResponse, account_id: &str) -> anyhow::Result<BalanceRow> {
    let info = body
        .balance_infos
        .iter()
        .find(|i| i.currency == "CNY")
        .or_else(|| body.balance_infos.first())
        .ok_or_else(|| anyhow::anyhow!("DeepSeek 余额响应 balance_infos 为空"))?;
    let total = parse_amount(&info.total_balance)
        .ok_or_else(|| anyhow::anyhow!("DeepSeek 余额响应 total_balance 解析失败"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(BalanceRow {
        account_id: account_id.into(),
        currency: if info.currency.is_empty() {
            "CNY".into()
        } else {
            info.currency.clone()
        },
        total,
        granted: parse_amount(&info.granted_balance),
        available: None,
        fetched_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方文档响应样例（api-docs.deepseek.com/api/get-user-balance）
    const DOC_RESP: &str = r#"{
      "is_available": true,
      "balance_infos": [
        {"currency": "CNY", "total_balance": "110.00", "granted_balance": "10.00", "topped_up_balance": "100.00"}
      ]
    }"#;

    #[test]
    fn test_parse_doc_response() {
        let body: BalanceResponse = serde_json::from_str(DOC_RESP).unwrap();
        let row = parse_balance(&body, "acc-1").unwrap();
        assert_eq!(row.account_id, "acc-1", "快照必须归属传入实例");
        assert_eq!(row.currency, "CNY");
        assert_eq!(row.total, 110.00);
        assert_eq!(row.granted, Some(10.00));
        assert_eq!(row.available, None, "无独立可用额概念，统一 None");
    }

    #[test]
    fn test_multi_currency_prefers_cny() {
        let body: BalanceResponse = serde_json::from_str(
            r#"{"is_available": true, "balance_infos": [
                {"currency": "USD", "total_balance": "1.00", "granted_balance": "0.00"},
                {"currency": "CNY", "total_balance": "88.80", "granted_balance": "8.80"}]}"#,
        )
        .unwrap();
        let row = parse_balance(&body, "acc-1").unwrap();
        assert_eq!(row.currency, "CNY", "多币种应优先取 CNY 条目");
        assert_eq!(row.total, 88.80);
    }

    #[test]
    fn test_parse_amount() {
        assert_eq!(parse_amount(&Some("110.00".into())), Some(110.0));
        assert_eq!(parse_amount(&Some(" 0 ".into())), Some(0.0), "应容忍首尾空白");
        assert_eq!(parse_amount(&Some("".into())), None, "空字符串按缺失处理");
        assert_eq!(parse_amount(&Some("abc".into())), None);
        assert_eq!(parse_amount(&None), None);
    }

    /// 集成：真实调用余额接口（手动：设 DEEPSEEK_API_KEY 后 cargo test -- --ignored）
    #[test]
    #[ignore]
    fn test_real_deepseek_fetch() {
        let Ok(key) = std::env::var("DEEPSEEK_API_KEY") else {
            println!("未设置 DEEPSEEK_API_KEY，跳过真实调用");
            return;
        };
        let p = DeepseekProvider::new("acc-real", BASE_DEEPSEEK, &key).unwrap();
        match p.fetch().unwrap() {
            QuotaSnapshot::Balance(row) => {
                println!("余额 {:.2} {}（赠金 {:?}）", row.total, row.currency, row.granted);
                assert!(row.total.is_finite(), "余额应为有限数");
                assert_eq!(row.account_id, "acc-real");
            }
            _ => panic!("DeepSeek 应返回余额口径快照"),
        }
    }
}
