//! OpenRouter 适配器：credits 余额＋近 30 天用量（M3-7，Balance/USD 口径）。
//! 接口与响应格式来自官方文档（openrouter.ai/docs，2026-09-25 核验）：
//!   GET {base}/api/v1/credits   → data.total_credits／data.total_usage（双双必填）
//!   GET {base}/api/v1/activity  → 近 30 个完整 UTC 天、按端点分组的用量行（免翻页）
//! 两端点均要求 Management Key（openrouter.ai/settings/management-keys 创建），
//! 普通推理 Key 返回 403「Only management keys can perform this operation」；
//! 官方建议为 Management Key 设过期时间，过期后 401「API key expired」。

use crate::provider::{describe_snapshot, http_client, ProviderAdapter, QuotaSnapshot};
use crate::store::BalanceRow;

/// OpenRouter 端点（credits/activity 均挂在主站 /api/v1 下）
pub const BASE_OPENROUTER: &str = "https://openrouter.ai";

pub struct OpenrouterProvider {
    /// 实例归属（快照入库 account_id）
    account_id: String,
    /// API origin（如 https://openrouter.ai）
    base: String,
    /// Management Key（非普通推理 Key；凭据 ⓘ 注明创建路径与过期提醒）
    key: String,
    /// 复用的 blocking 客户端：共享连接池与 TLS 会话，自带 5s 超时
    http: reqwest::blocking::Client,
}

impl OpenrouterProvider {
    pub fn new(account_id: &str, base: &str, key: &str) -> anyhow::Result<Self> {
        Ok(Self {
            account_id: account_id.to_string(),
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            http: http_client("OpenRouter")?,
        })
    }

    /// 近 30 天用量汇总（test 摘要附带）：activity 按端点分组免翻页，
    /// 客户端对行求和即可
    fn fetch_activity(&self) -> anyhow::Result<(f64, u64)> {
        let url = format!("{}/api/v1/activity", self.base);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.key)
            .header("Accept", "application/json")
            .send()?;
        if !resp.status().is_success() {
            anyhow::bail!("OpenRouter activity 接口 HTTP {}", resp.status());
        }
        let body: ActivityResponse = resp.json()?;
        Ok(parse_activity(&body))
    }
}

impl ProviderAdapter for OpenrouterProvider {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    fn fetch(&self) -> anyhow::Result<QuotaSnapshot> {
        let url = format!("{}/api/v1/credits", self.base);
        // 失败必须留痕（审查 1.1 惯例）：此处降级为"显示最近快照"，不允许无痕降级
        let result = (|| -> anyhow::Result<BalanceRow> {
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.key)
                .header("Accept", "application/json")
                .send()?;
            if !resp.status().is_success() {
                // 动作导向提示（moonshot 401 文案先例）：403＝普通推理 Key 无权限；
                // 401＝key 无效或已过期（Management Key 可设过期时间，到期前需重建）
                let st = resp.status();
                let hint = if st == reqwest::StatusCode::FORBIDDEN {
                    "（credits 端点要求 Management Key，普通推理 Key 无权限；openrouter.ai/settings/management-keys 创建）"
                } else if st == reqwest::StatusCode::UNAUTHORIZED {
                    "（key 无效或已过期；Management Key 可设过期时间，到期前需重建更换）"
                } else {
                    ""
                };
                anyhow::bail!("OpenRouter credits 接口 HTTP {st}{hint}");
            }
            let body: CreditsResponse = resp.json()?;
            parse_credits(&body, &self.account_id)
        })();
        match result {
            Ok(row) => {
                log::debug!("OpenRouter 余额查询成功：{:.2} {}", row.total, row.currency);
                Ok(QuotaSnapshot::Balance(row))
            }
            Err(e) => {
                log::warn!("OpenRouter 余额查询失败（降级为最近快照）：{e}");
                Err(e)
            }
        }
    }

    /// 检测摘要覆盖（mod.rs trait 注释预留点）：credits 余额＋近 30 天
    /// activity 汇总；activity 失败不拖垮检测主结果，降级为纯余额摘要并留痕
    fn test(&self) -> anyhow::Result<String> {
        let base_line = describe_snapshot(&self.fetch()?);
        match self.fetch_activity() {
            Ok((usage, requests)) => {
                Ok(format!("{base_line}；近 30 天已用 ${usage:.2}（{requests} 次请求）"))
            }
            Err(e) => {
                log::warn!("OpenRouter activity 查询失败（检测降级为纯余额摘要）：{e}");
                Ok(base_line)
            }
        }
    }
}

// ---------- 响应解析（结构来自官方文档样例） ----------

#[derive(serde::Deserialize)]
struct CreditsResponse {
    data: CreditsData,
}

#[derive(serde::Deserialize)]
struct CreditsData {
    /// 充值总额（含赠送 credits），USD
    #[serde(default)]
    total_credits: Option<f64>,
    /// 历史消费总额，USD
    #[serde(default)]
    total_usage: Option<f64>,
}

#[derive(serde::Deserialize)]
struct ActivityResponse {
    /// 近 30 个完整 UTC 天、按端点分组的用量行（免翻页）
    #[serde(default)]
    data: Vec<ActivityItem>,
}

#[derive(serde::Deserialize)]
struct ActivityItem {
    /// 该行 OpenRouter credits 消费（USD）。BYOK 外部消费单列
    /// byok_usage_inference、不走本账户余额，显式不取
    #[serde(default)]
    usage: f64,
    /// 该行请求次数
    #[serde(default)]
    requests: u64,
}

/// 解析 credits 响应 → 余额快照（归属传入实例）。剩余余额 = 充值总额 −
/// 历史消费（消费大于充值＝欠费，允许为负，与 moonshot 现金余额负值同语义）。
/// OpenRouter 无赠金/可用额拆分端点：granted/available 统一 None（06 §2.2 口径）
fn parse_credits(body: &CreditsResponse, account_id: &str) -> anyhow::Result<BalanceRow> {
    let credits = body
        .data
        .total_credits
        .ok_or_else(|| anyhow::anyhow!("OpenRouter credits 响应缺少 total_credits"))?;
    let usage = body
        .data
        .total_usage
        .ok_or_else(|| anyhow::anyhow!("OpenRouter credits 响应缺少 total_usage"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Ok(BalanceRow {
        account_id: account_id.into(),
        currency: "USD".into(),
        total: credits - usage,
        granted: None,
        available: None,
        fetched_at: now,
    })
}

/// activity 行求和：(总消费 USD，总请求次数)。usage 缺行按 0 计（容忍部分残缺，
/// 宁少报不编造）
fn parse_activity(body: &ActivityResponse) -> (f64, u64) {
    body.data
        .iter()
        .fold((0.0, 0u64), |(u, r), item| (u + item.usage, r + item.requests))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方文档响应样例（openrouter.ai/docs/api/api-reference/credits/get-remaining-credits）
    const DOC_CREDITS: &str = r#"{
      "data": {
        "total_credits": 100.5,
        "total_usage": 25.75
      }
    }"#;

    /// 官方文档 activity 响应样例（字段有节选：本适配器只消费 usage/requests）
    const DOC_ACTIVITY: &str = r#"{
      "data": [
        {
          "byok_usage_inference": 0.012,
          "completion_tokens": 125,
          "date": "2025-08-24",
          "endpoint_id": "550e8400-e29b-41d4-a716-446655440000",
          "model": "openai/gpt-4.1",
          "model_permaslug": "openai/gpt-4.1-2025-04-14",
          "prompt_tokens": 50,
          "provider_name": "OpenAI",
          "reasoning_tokens": 25,
          "requests": 5,
          "usage": 0.015
        },
        {
          "date": "2025-08-23",
          "model": "anthropic/claude-sonnet-4",
          "model_permaslug": "anthropic/claude-sonnet-4",
          "endpoint_id": "550e8400-e29b-41d4-a716-446655440001",
          "provider_name": "Anthropic",
          "requests": 20,
          "usage": 0.1,
          "byok_usage_inference": 0.0,
          "prompt_tokens": 900,
          "completion_tokens": 1200,
          "reasoning_tokens": 0
        }
      ]
    }"#;

    #[test]
    fn test_parse_doc_credits() {
        let body: CreditsResponse = serde_json::from_str(DOC_CREDITS).unwrap();
        let row = parse_credits(&body, "acc-1").unwrap();
        assert_eq!(row.account_id, "acc-1", "快照必须归属传入实例");
        assert_eq!(row.currency, "USD", "OpenRouter 余额按 USD 计");
        assert!((row.total - 74.75).abs() < 1e-9, "剩余余额＝充值总额−历史消费");
        assert_eq!(row.granted, None, "官方无赠金拆分端点，统一 None");
        assert_eq!(row.available, None, "无独立可用额概念，统一 None");
    }

    #[test]
    fn test_overdraft_negative_is_allowed() {
        // 消费大于充值＝欠费：允许为负（与 moonshot 现金余额负值同语义），
        // UI 层负数走"欠费"红字路径
        let body: CreditsResponse = serde_json::from_str(
            r#"{"data": {"total_credits": 1.0, "total_usage": 2.5}}"#,
        )
        .unwrap();
        let row = parse_credits(&body, "acc-1").unwrap();
        assert!((row.total + 1.5).abs() < 1e-9);
    }

    #[test]
    fn test_missing_field_errors() {
        let body: CreditsResponse =
            serde_json::from_str(r#"{"data": {"total_credits": 10.0}}"#).unwrap();
        assert!(parse_credits(&body, "acc-1").is_err(), "缺 total_usage 必须报错，不冒充 0");
    }

    #[test]
    fn test_parse_activity_sum() {
        let body: ActivityResponse = serde_json::from_str(DOC_ACTIVITY).unwrap();
        let (usage, requests) = parse_activity(&body);
        assert!((usage - 0.115).abs() < 1e-9, "对行求和得近 30 天总消费");
        assert_eq!(requests, 25);
    }

    #[test]
    fn test_parse_activity_empty() {
        let body: ActivityResponse = serde_json::from_str(r#"{"data": []}"#).unwrap();
        let (usage, requests) = parse_activity(&body);
        assert_eq!((usage, requests), (0.0, 0), "空数据返回零和，不报错");
    }

    /// 集成：真实调用 credits＋activity（手动：设 OPENROUTER_MGMT_KEY 后
    /// cargo test -- --ignored）
    #[test]
    #[ignore]
    fn test_real_openrouter_fetch() {
        let Ok(key) = std::env::var("OPENROUTER_MGMT_KEY") else {
            println!("未设置 OPENROUTER_MGMT_KEY，跳过真实调用");
            return;
        };
        let p = OpenrouterProvider::new("acc-real", BASE_OPENROUTER, &key).unwrap();
        match p.fetch().unwrap() {
            QuotaSnapshot::Balance(row) => {
                println!("余额 {:.2} {}（充值总额−历史消费）", row.total, row.currency);
                assert!(row.total.is_finite(), "余额应为有限数");
                assert_eq!(row.account_id, "acc-real");
            }
            _ => panic!("OpenRouter 应返回余额口径快照"),
        }
        println!("检测摘要：{}", p.test().unwrap());
    }
}
