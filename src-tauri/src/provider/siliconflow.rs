//! 硅基流动 SiliconFlow 适配器：查询货币余额（M3-3，Balance 口径）。
//! 接口与响应格式来自官方文档（docs.siliconflow.com get-user-info，
//! 2026-09-24 核验）：
//!   GET {base}/v1/user/info   Header: Authorization: Bearer <key>
//!   三个余额字段均为字符串金额（如 "88.88"）；2026-06 起 name/email 恒返回
//!   空串（官方公告，勿依赖，故不解析）；国际站 api.siliconflow.com 经实例
//!   base 覆盖建独立实例。

use crate::provider::{http_client, ProviderAdapter, QuotaSnapshot};
use crate::store::BalanceRow;

/// 硅基流动国内站端点（2026-10-09 调研确证国际站为 api.siliconflow.com，
/// 官方已正式启用 .com 端点）
pub const BASE_SILICONFLOW: &str = "https://api.siliconflow.cn";
/// 硅基流动国际站端点（变体机制 2026-10-10 入注册表；账号独立、余额币种 USD）
pub const BASE_SILICONFLOW_INTL: &str = "https://api.siliconflow.com";

pub struct SiliconflowProvider {
    /// 实例归属（快照入库 account_id）
    account_id: String,
    /// API origin（如 https://api.siliconflow.cn）
    base: String,
    /// 硅基流动 API key（Bearer）
    key: String,
    /// 余额币种（双站变体：国内 CNY／国际 USD，由 build_adapter 按变体传入）
    currency: &'static str,
    /// 复用的 blocking 客户端（审查 2.2.5）：共享连接池与 TLS 会话，自带 5s 超时
    http: reqwest::blocking::Client,
}

impl SiliconflowProvider {
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
            http: http_client("硅基流动")?,
        })
    }
}

impl ProviderAdapter for SiliconflowProvider {
    fn id(&self) -> &'static str {
        "siliconflow"
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    fn fetch(&self) -> anyhow::Result<QuotaSnapshot> {
        let url = format!("{}/v1/user/info", self.base);
        // 失败必须留痕（审查 1.1）：此处降级为"显示最近快照"，但不允许无痕降级
        let result = (|| -> anyhow::Result<BalanceRow> {
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.key)
                .header("Accept", "application/json")
                .send()?;
            if !resp.status().is_success() {
                anyhow::bail!("硅基流动余额接口 HTTP {}", resp.status());
            }
            let body: UserInfoResponse = resp.json()?;
            parse_balance(&body, &self.account_id, self.currency)
        })();
        match result {
            Ok(row) => {
                log::debug!("硅基流动余额查询成功：{:.2} {}", row.total, row.currency);
                Ok(QuotaSnapshot::Balance(row))
            }
            Err(e) => {
                log::warn!("硅基流动余额查询失败（降级为最近快照）：{e}");
                Err(e)
            }
        }
    }
}

// ---------- 响应解析（结构来自官方文档样例） ----------

#[derive(serde::Deserialize)]
struct UserInfoResponse {
    /// 业务码（20000 = 成功；HTTP 200 但业务失败时据此拦截，缺失时不拦由 data 兜底）
    #[serde(default)]
    code: Option<i64>,
    /// 请求状态标志（与 code 重复，留档）
    #[serde(default)]
    #[allow(dead_code)]
    status: bool,
    /// 用户信息（余额字段在此；name/email/image 已恒返回空串，不解析）
    data: Option<UserData>,
}

#[derive(serde::Deserialize)]
struct UserData {
    /// 当前账户余额（赠金/赠送余额），字符串金额
    #[serde(default)]
    balance: Option<String>,
    /// 充值余额，字符串金额。BalanceRow 无"充值"列：现金部分可由
    /// total - granted 反推，此处仅留档不单独入库
    #[serde(default)]
    #[serde(rename = "chargeBalance")]
    #[allow(dead_code)]
    charge_balance: Option<String>,
    /// 总余额（充值与赠送合计），字符串金额
    #[serde(default)]
    #[serde(rename = "totalBalance")]
    total_balance: Option<String>,
    /// 账户状态（如 "normal"；异常状态不影响余额入库，留档）
    #[serde(default)]
    #[serde(rename = "status")]
    #[allow(dead_code)]
    account_status: Option<String>,
}

/// 字符串金额解析（官方返回如 "88.88"）；解析不出返回 None——宁缺毋滥，
/// UI 显示"--"，不拿脏值冒充金额
fn parse_amount(v: &Option<String>) -> Option<f64> {
    v.as_deref()?.trim().parse::<f64>().ok()
}

/// 解析响应 → 余额快照（归属传入实例）。available 统一填 None（06 §2.2 回写）：
/// totalBalance 即可用总额，无独立于总额的可用额概念。
/// 币种由调用方按站点变体传入（国内 CNY／国际 USD），不在此硬编码
fn parse_balance(
    body: &UserInfoResponse,
    account_id: &str,
    currency: &'static str,
) -> anyhow::Result<BalanceRow> {
    if let Some(c) = body.code {
        if c != 20000 {
            anyhow::bail!("硅基流动余额响应业务码异常：{c}");
        }
    }
    let data = body
        .data
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("硅基流动余额响应缺少 data"))?;
    let total = parse_amount(&data.total_balance)
        .ok_or_else(|| anyhow::anyhow!("硅基流动余额响应 totalBalance 解析失败"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(BalanceRow {
        account_id: account_id.into(),
        currency: currency.into(),
        total,
        granted: parse_amount(&data.balance),
        available: None,
        fetched_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方文档响应样例（docs.siliconflow.com get-user-info；余额字段以外
    /// 按 2026-06 公告为空串）
    const DOC_RESP: &str = r#"{
      "code": 20000,
      "message": "OK",
      "status": true,
      "data": {
        "id": "userid",
        "name": "",
        "image": "",
        "email": "",
        "isAdmin": false,
        "balance": "0.88",
        "status": "normal",
        "introduction": "",
        "role": "",
        "chargeBalance": "88.00",
        "totalBalance": "88.88"
      }
    }"#;

    #[test]
    fn test_parse_doc_response() {
        let body: UserInfoResponse = serde_json::from_str(DOC_RESP).unwrap();
        let row = parse_balance(&body, "acc-1", "CNY").unwrap();
        assert_eq!(row.account_id, "acc-1", "快照必须归属传入实例");
        assert_eq!(row.currency, "CNY");
        assert!((row.total - 88.88).abs() < 1e-9, "total 应取 totalBalance");
        assert!((row.granted.unwrap() - 0.88).abs() < 1e-9, "granted 应取 balance（赠金）");
        assert_eq!(row.available, None, "无独立可用额概念，统一 None");
    }

    #[test]
    fn test_business_code_and_missing_data() {
        // HTTP 200 但业务码非 20000：必须拦截，不得把空数据当成功
        let body: UserInfoResponse =
            serde_json::from_str(r#"{"code": 20001, "message": "error"}"#).unwrap();
        assert!(parse_balance(&body, "acc-1", "CNY").is_err(), "业务码非 20000 应报错");
        // 业务码为 20000 但缺 data：data 校验兜底拦截
        let body: UserInfoResponse = serde_json::from_str(r#"{"code": 20000}"#).unwrap();
        assert!(parse_balance(&body, "acc-1", "CNY").is_err(), "缺少 data 应报错");
    }

    #[test]
    fn test_string_amount_parsing() {
        assert_eq!(parse_amount(&Some("88.88".into())), Some(88.88));
        assert_eq!(parse_amount(&Some(" 0.00 ".into())), Some(0.0), "应容忍首尾空白");
        assert_eq!(parse_amount(&Some("".into())), None, "空字符串按缺失处理");
        assert_eq!(parse_amount(&None), None);
        // 赠金字段脏值：granted 降级 None，不影响 total 入库
        let body: UserInfoResponse = serde_json::from_str(
            r#"{"code": 20000, "data": {"balance": "n/a", "totalBalance": "88.88"}}"#,
        )
        .unwrap();
        let row = parse_balance(&body, "acc-1", "CNY").unwrap();
        assert_eq!(row.total, 88.88);
        assert_eq!(row.granted, None);
    }

    #[test]
    fn test_intl_variant_currency_is_usd() {
        // 双站变体（2026-10-10）：国际站余额币种 USD 由调用方传入，解析层不硬编码
        let body: UserInfoResponse = serde_json::from_str(DOC_RESP).unwrap();
        let row = parse_balance(&body, "acc-intl", "USD").unwrap();
        assert_eq!(row.currency, "USD");
    }

    /// 集成：真实调用余额接口（手动：设 SILICONFLOW_API_KEY 后 cargo test -- --ignored）
    #[test]
    #[ignore]
    fn test_real_siliconflow_fetch() {
        let Ok(key) = std::env::var("SILICONFLOW_API_KEY") else {
            println!("未设置 SILICONFLOW_API_KEY，跳过真实调用");
            return;
        };
        let p = SiliconflowProvider::new("acc-real", BASE_SILICONFLOW, &key, "CNY").unwrap();
        match p.fetch().unwrap() {
            QuotaSnapshot::Balance(row) => {
                println!("余额 {:.2} {}（赠金 {:?}）", row.total, row.currency, row.granted);
                assert!(row.total.is_finite(), "余额应为有限数");
                assert_eq!(row.account_id, "acc-real");
            }
            _ => panic!("硅基流动应返回余额口径快照"),
        }
    }
}
