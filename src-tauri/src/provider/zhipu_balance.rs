//! 智谱开放平台按量账户适配器：查询货币余额（M3-8，Balance/CNY 口径）。
//! 端点为未文档化内部接口（best-effort，来源 CodexBar PR 实测 2026-08），
//! 本适配器响应结构经本机装机实测核实（2026-09-25，单测样例即实测响应）：
//!   GET {base}/api/biz/account/query-customer-account-report   Bearer <key>
//! 与 glm（Coding Plan 窗口口径）同一平台不同账户视角：按量余额随调用扣减，
//! Coding Plan 订阅窗口互不影响。z.ai 国际站无等价接口（05 调研），仅国内站。
//! 接口无官方文档，随时可能变更——失败按常规降级留痕，不阻塞其他实例。

use crate::provider::{http_client, ProviderAdapter, QuotaSnapshot};
use crate::store::BalanceRow;

/// 智谱主站端点（按量接口挂 www.bigmodel.cn，与 open.bigmodel.cn 的
/// Coding Plan monitor 接口域名不同）
pub const BASE_ZHIPU_BALANCE: &str = "https://www.bigmodel.cn";

pub struct ZhipuBalanceProvider {
    /// 实例归属（快照入库 account_id）
    account_id: String,
    /// API origin（如 https://www.bigmodel.cn）
    base: String,
    /// 智谱开放平台 API Key（Bearer）
    key: String,
    /// 复用的 blocking 客户端：共享连接池与 TLS 会话，自带 5s 超时
    http: reqwest::blocking::Client,
}

impl ZhipuBalanceProvider {
    pub fn new(account_id: &str, base: &str, key: &str) -> anyhow::Result<Self> {
        Ok(Self {
            account_id: account_id.to_string(),
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            http: http_client("GLM 按量")?,
        })
    }
}

impl ProviderAdapter for ZhipuBalanceProvider {
    fn id(&self) -> &'static str {
        "zhipu-balance"
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    fn fetch(&self) -> anyhow::Result<QuotaSnapshot> {
        let url = format!("{}/api/biz/account/query-customer-account-report", self.base);
        // 失败必须留痕（审查 1.1 惯例）：此处降级为"显示最近快照"，不允许无痕降级
        let result = (|| -> anyhow::Result<BalanceRow> {
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.key)
                .header("Accept", "application/json")
                .send()?;
            if !resp.status().is_success() {
                anyhow::bail!("GLM 按量接口 HTTP {}", resp.status());
            }
            let body: ReportResponse = resp.json()?;
            parse_balance(&body, &self.account_id)
        })();
        match result {
            Ok(row) => {
                log::debug!("GLM 按量余额查询成功：{:.2} {}", row.total, row.currency);
                Ok(QuotaSnapshot::Balance(row))
            }
            Err(e) => {
                log::warn!("GLM 按量余额查询失败（降级为最近快照）：{e}");
                Err(e)
            }
        }
    }
}

// ---------- 响应解析（结构来自装机实测，2026-09-25） ----------

#[derive(serde::Deserialize)]
struct ReportResponse {
    /// 业务码（200 = 成功；HTTP 恒 200 但业务失败时据此拦截，如 1001 = 未带鉴权）
    #[serde(default)]
    code: i64,
    /// 业务结果标志（与 code 同步，解析只认 code；留档）
    #[serde(default)]
    #[allow(dead_code)]
    success: bool,
    /// 业务提示（成功"操作成功"；中文 UTF-8）
    #[serde(default)]
    #[allow(dead_code)]
    msg: String,
    #[serde(default)]
    data: Option<AccountReport>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountReport {
    /// 总余额（元），可为负＝欠费（实测欠费账户返回 -0.35）
    #[serde(default)]
    balance: Option<f64>,
    /// 赠送金额（元），未赠送为 0
    #[serde(default)]
    give_amount: Option<f64>,
    /// 可用余额（元）：独立字段（扣除冻结等），与 balance 可不同——智谱有
    /// 独立可用额概念，真值入库（06 §2.2"available 统一 None"的例外，已回写）
    #[serde(default)]
    available_balance: Option<f64>,
    /// 充值总额/累计消费/冻结/今日消费/授信等字段不入库：UI 无对应展示位，
    /// 留档不解析（授信 creditStatus=NOT_OPEN 时 credit 系字段恒 null）
    #[serde(default)]
    #[allow(dead_code)]
    recharge_amount: Option<f64>,
}

/// 解析响应 → 余额快照（归属传入实例）。业务码非 200 必须拦截（HTTP 恒 200），
/// 不得当成功；金额为数字（实测含 0E-9 科学计数法与 9 位小数精度，f64 无压力）
fn parse_balance(body: &ReportResponse, account_id: &str) -> anyhow::Result<BalanceRow> {
    if body.code != 200 {
        anyhow::bail!("GLM 按量响应业务码异常：{}（{}）", body.code, body.msg);
    }
    let data = body
        .data
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("GLM 按量响应缺少 data"))?;
    let balance = data
        .balance
        .ok_or_else(|| anyhow::anyhow!("GLM 按量响应缺少 balance"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(BalanceRow {
        account_id: account_id.into(),
        currency: "CNY".into(),
        total: balance,
        granted: data.give_amount,
        available: data.available_balance,
        fetched_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 装机实测响应样例（2026-09-25，欠费账户：balance/availableBalance 为负；
    /// frozenBalance 为 0E-9 科学计数法；today/credit 系字段 null）
    const REAL_RESP: &str = r#"{
      "code": 200,
      "msg": "操作成功",
      "success": true,
      "data": {
        "balance": -0.350000000,
        "rechargeAmount": 0.000000,
        "giveAmount": 0.000000,
        "totalSpendAmount": 0.350000000,
        "todaySpendAmount": null,
        "availableBalance": -0.350000000,
        "frozenBalance": 0E-9,
        "creditBalance": null,
        "availableCreditBalance": null,
        "creditStatus": "NOT_OPEN",
        "modelSpendAmountList": null,
        "isKA": false
      }
    }"#;

    #[test]
    fn test_parse_real_response() {
        let body: ReportResponse = serde_json::from_str(REAL_RESP).unwrap();
        let row = parse_balance(&body, "acc-1").unwrap();
        assert_eq!(row.account_id, "acc-1", "快照必须归属传入实例");
        assert_eq!(row.currency, "CNY");
        assert!((row.total + 0.35).abs() < 1e-9, "欠费账户余额为负");
        assert_eq!(row.granted, Some(0.0), "未赠送为 0 非 null");
        assert!(
            (row.available.unwrap() + 0.35).abs() < 1e-9,
            "available 取 availableBalance 真值"
        );
    }

    #[test]
    fn test_business_code_intercepted() {
        // 实测无鉴权响应（HTTP 恒 200，业务码 1001 拦截不得当成功）
        let body: ReportResponse = serde_json::from_str(
            r#"{"code":1001,"msg":"Header 未收到 Authorization 参数","success":false}"#,
        )
        .unwrap();
        assert!(parse_balance(&body, "acc-1").is_err(), "业务码非 200 必须报错");
    }

    #[test]
    fn test_missing_data_and_balance() {
        // data 缺失：settings 兼容旧响应形态的防御（字段全缺不冒充 0）
        let body: ReportResponse =
            serde_json::from_str(r#"{"code":200,"success":true}"#).unwrap();
        assert!(parse_balance(&body, "acc-1").is_err());
        let body: ReportResponse =
            serde_json::from_str(r#"{"code":200,"success":true,"data":{}}"#).unwrap();
        assert!(parse_balance(&body, "acc-1").is_err());
    }

    /// 集成：真实调用按量接口（手动：设 ZHIPU_BALANCE_API_KEY 后
    /// cargo test -- --ignored）
    #[test]
    #[ignore]
    fn test_real_zhipu_balance_fetch() {
        let Ok(key) = std::env::var("ZHIPU_BALANCE_API_KEY") else {
            println!("未设置 ZHIPU_BALANCE_API_KEY，跳过真实调用");
            return;
        };
        let p = ZhipuBalanceProvider::new("acc-real", BASE_ZHIPU_BALANCE, &key).unwrap();
        match p.fetch().unwrap() {
            QuotaSnapshot::Balance(row) => {
                println!(
                    "余额 {:.2} {}（赠金 {:?}，可用 {:?}）",
                    row.total, row.currency, row.granted, row.available
                );
                assert!(row.total.is_finite(), "余额应为有限数");
                assert_eq!(row.account_id, "acc-real");
            }
            _ => panic!("GLM 按量应返回余额口径快照"),
        }
    }
}
