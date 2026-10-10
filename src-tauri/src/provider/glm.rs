//! GLM Coding Plan 适配器：查询 5 小时/每周积分窗口用量与重置时间。
//! 接口与响应格式来自本机实测（2026-09-16，见 docs/01-RESEARCH.md §7/§9）：
//!   GET {origin}/api/monitor/usage/quota/limit   Header: Authorization: <裸key>
//!   data.limits[] 中 TOKENS_LIMIT+number=5 → 5h 窗口；number=1&unit=6 → 周窗口；
//!   percentage 为已用百分比；nextResetTime 为 Unix 毫秒。

use crate::provider::{
    http_client, DiscoveredCreds, ProviderAdapter, ProviderDiscoverer, QuotaSnapshot,
};
use crate::store::QuotaRow;

/// GLM 平台端点（国内/国际）
pub const BASE_BIGMODEL: &str = "https://open.bigmodel.cn";
pub const BASE_ZAI: &str = "https://api.z.ai";

pub struct GlmProvider {
    /// 实例归属（快照入库 account_id）
    account_id: String,
    /// API origin(如 https://open.bigmodel.cn)
    base: String,
    /// Coding Plan API key（与 ANTHROPIC_AUTH_TOKEN 同值）
    token: String,
    /// 复用的 blocking 客户端（审查 2.2.5）：每 5min 一次的查询共享连接池与
    /// TLS 会话，替代旧的"每次请求新建 Client"；自带 5s 超时
    http: reqwest::blocking::Client,
}

impl GlmProvider {
    pub fn new(account_id: &str, base: &str, token: &str) -> anyhow::Result<Self> {
        Ok(Self {
            account_id: account_id.to_string(),
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            // 5s 总超时→重试→连接超时→Err（绝不无超时兜底），全适配器共用
            http: http_client("GLM")?,
        })
    }
}

/// GLM 凭据发现器（06-PLAN §4.3 泛化）：从本机环境发现凭据，只当建议者，
/// 一切以实例表为准。发现链（优先级从高到低）：
/// ① 环境变量 → ② ~\.claude\suppliers.json 自动发现
/// （claude-menu 的供应商配置文件，选 base 含 bigmodel.cn / z.ai 的条目）
pub struct GlmDiscoverer;

impl GlmDiscoverer {
    /// base 归一：去掉可能的 anthropic 路径后缀与结尾斜杠
    fn normalize_base(base: &str) -> String {
        base.trim_end_matches('/')
            .trim_end_matches("/api/anthropic")
            .to_string()
    }

    /// base 是否为 GLM 平台（bigmodel/z.ai）——2026-10-10 双站变体机制起改查
    /// 注册表变体端点（GLM_DEF.variants）单真值源，域名不再散落硬编码；
    /// 新增 GLM 站点只改注册表，此处自动跟随
    fn is_glm_base(base: &str) -> bool {
        crate::provider::kind_of("glm")
            .map(|k| {
                k.variants
                    .iter()
                    .any(|v| base.contains(crate::provider::host_of(v.default_base)))
            })
            .unwrap_or(false)
    }
}

impl ProviderDiscoverer for GlmDiscoverer {
    fn kind_id(&self) -> &'static str {
        "glm"
    }

    fn discover(&self) -> Vec<DiscoveredCreds> {
        // ① 环境变量
        if let (Ok(base), Ok(tok)) = (
            std::env::var("ANTHROPIC_BASE_URL"),
            std::env::var("ANTHROPIC_AUTH_TOKEN"),
        ) {
            if !tok.is_empty() && Self::is_glm_base(&base) {
                return vec![DiscoveredCreds {
                    base: Self::normalize_base(&base),
                    key: tok,
                    source: "环境变量",
                }];
            }
        }
        // ② suppliers.json（键名带 "env:" 前缀）
        if let Some(c) = discover_from_suppliers() {
            return vec![c];
        }
        vec![]
    }
}

/// 从 claude-menu 的 ~\.claude\suppliers.json 发现 GLM 凭据（只读）
fn discover_from_suppliers() -> Option<DiscoveredCreds> {
    let mut path = std::path::PathBuf::from(std::env::var_os("USERPROFILE")?);
    path.push(".claude");
    path.push("suppliers.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let Some(obj) = root.as_object() else { return None };
    for (_name, entry) in obj {
        let Some(fields) = entry.as_object() else { continue };
        let get = |k: &str| fields.get(&format!("env:{k}")).and_then(|v| v.as_str());
        let (Some(base), Some(tok)) = (get("ANTHROPIC_BASE_URL"), get("ANTHROPIC_AUTH_TOKEN")) else {
            continue;
        };
        if !tok.is_empty() && GlmDiscoverer::is_glm_base(base) {
            return Some(DiscoveredCreds {
                base: GlmDiscoverer::normalize_base(base),
                key: tok.to_string(),
                source: "claude-menu 配置",
            });
        }
    }
    None
}

impl ProviderAdapter for GlmProvider {
    fn id(&self) -> &'static str {
        "glm"
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    fn fetch(&self) -> anyhow::Result<QuotaSnapshot> {
        let url = format!("{}/api/monitor/usage/quota/limit", self.base);
        // 失败必须留痕（审查 1.1）：此处降级为"显示最近快照"，但不允许无痕降级
        let result = (|| -> anyhow::Result<Vec<QuotaRow>> {
            let resp = self
                .http
                .get(&url)
                .header("Authorization", &self.token)
                .header("Accept", "application/json")
                .send()?;
            if !resp.status().is_success() {
                anyhow::bail!("GLM 额度接口 HTTP {}", resp.status());
            }
            let body: QuotaResponse = resp.json()?;
            parse_quota(&body, &self.account_id)
        })();
        match result {
            Ok(rows) => {
                log::debug!("GLM 额度查询成功：{} 条", rows.len());
                Ok(QuotaSnapshot::Windows(rows))
            }
            Err(e) => {
                log::warn!("GLM 额度查询失败（降级为最近快照）：{e}");
                Err(e)
            }
        }
    }
}

// ---------- 响应解析（结构来自 2026-09-16 实测） ----------

#[derive(serde::Deserialize)]
struct QuotaResponse {
    /// 应答成功标志（当前解析只看 data；字段留档便于排查接口异常）
    #[serde(default)]
    #[allow(dead_code)]
    success: bool,
    data: Option<QuotaData>,
}

#[derive(serde::Deserialize)]
struct QuotaData {
    /// 套餐档位：lite/pro/max
    #[serde(default)]
    #[allow(dead_code)]
    level: Option<String>,
    #[serde(default)]
    limits: Vec<LimitItem>,
}

#[derive(serde::Deserialize)]
struct LimitItem {
    /// TOKENS_LIMIT（积分窗口）| TIME_LIMIT（MCP 工具，M1 处理）
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    number: Option<i64>,
    #[serde(default)]
    unit: Option<i64>,
    #[serde(default)]
    usage: Option<i64>,
    #[serde(default)]
    #[serde(rename = "currentValue")]
    /// 已用绝对量：无 total 无法换算百分比，仅留档（见 calc_percent 注释）
    #[allow(dead_code)]
    current_value: Option<i64>,
    #[serde(default)]
    remaining: Option<i64>,
    /// 已用百分比（官方口径，实测为"已用"而非"剩余"）
    #[serde(default)]
    percentage: Option<f64>,
    #[serde(default)]
    #[serde(rename = "nextResetTime")]
    next_reset_time: Option<i64>,
}

/// 解析响应 → 额度快照（5h + weekly，归属传入实例）
fn parse_quota(body: &QuotaResponse, account_id: &str) -> anyhow::Result<Vec<QuotaRow>> {
    let data = body
        .data
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("GLM 额度响应缺少 data"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut out = vec![];
    for item in &data.limits {
        if item.kind != "TOKENS_LIMIT" {
            continue; // TIME_LIMIT(MCP)M1 纳入
        }
        // number=5 → 5h 窗口；number=1（周）→ weekly
        let kind = match (item.number, item.unit) {
            (Some(5), _) => "5h",
            (Some(1), Some(6)) => "weekly",
            _ => continue, // 未知窗口类型，忽略以容忍接口演进
        };
        // 已用百分比：官方 percentage 优先，缺失时按 usage/remaining 计算
        let used_percent = item
            .percentage
            .or_else(|| calc_percent(item.usage, item.remaining));
        out.push(QuotaRow {
            provider: "glm".into(),
            account_id: Some(account_id.into()),
            window_kind: kind.into(),
            used_percent,
            used_tokens: None, // TOKENS_LIMIT 仅返回百分比，无绝对量
            reset_at: item.next_reset_time,
            fetched_at: now,
        });
    }
    if out.is_empty() {
        anyhow::bail!("GLM 额度响应无可识别的 TOKENS_LIMIT 条目");
    }
    Ok(out)
}

/// percentage 缺失时的兜底：仅 usage/remaining 口径能换算出真实百分比；
/// currentValue 是已用绝对量而非百分比，无 total 无法换算——宁缺毋滥返回
/// None（UI 显示 "--"），不拿绝对量冒充百分比误导展示
fn calc_percent(usage: Option<i64>, remaining: Option<i64>) -> Option<f64> {
    if let (Some(u), Some(r)) = (usage, remaining) {
        let total = u + r;
        if total > 0 {
            return Some((u as f64 / total as f64) * 100.0);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实测响应样例（2026-09-16 本机，数值已核对：89%=5h，52%=周）
    const REAL_RESP: &str = r#"{
      "code": 200, "msg": "操作成功", "success": true,
      "data": {
        "level": "pro",
        "limits": [
          {"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":16,"remaining":984,"percentage":1,"nextResetTime":1790407328997},
          {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":89,"nextResetTime":1789562676650},
          {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":52,"nextResetTime":1789716128992}
        ]
      }
    }"#;

    #[test]
    fn test_parse_real_response() {
        let body: QuotaResponse = serde_json::from_str(REAL_RESP).unwrap();
        let rows = parse_quota(&body, "acc-1").unwrap();
        assert_eq!(rows.len(), 2, "TIME_LIMIT 应被忽略");
        let h5 = rows.iter().find(|r| r.window_kind == "5h").unwrap();
        assert_eq!(h5.used_percent, Some(89.0));
        assert_eq!(h5.reset_at, Some(1789562676650));
        let week = rows.iter().find(|r| r.window_kind == "weekly").unwrap();
        assert_eq!(week.used_percent, Some(52.0));
        assert!(rows.iter().all(|r| r.provider == "glm"));
        assert!(rows.iter().all(|r| r.account_id.as_deref() == Some("acc-1")),
            "快照必须归属传入实例");
    }

    #[test]
    fn test_fallback_percent() {
        assert_eq!(calc_percent(Some(16), Some(984)), Some(1.6));
        // currentValue 绝对量不能冒充百分比：无法换算时返回 None
        assert_eq!(calc_percent(None, None), None);
        assert_eq!(calc_percent(Some(0), Some(0)), None);
    }

    #[test]
    fn test_base_normalize() {
        assert_eq!(
            GlmDiscoverer::normalize_base("https://open.bigmodel.cn/api/anthropic/"),
            "https://open.bigmodel.cn"
        );
        assert!(GlmDiscoverer::is_glm_base("https://api.z.ai/v1"));
        assert!(!GlmDiscoverer::is_glm_base("https://api.deepseek.com"));
    }

    /// 集成：真实调用 Monitor API（手动：cargo test -- --ignored）
    #[test]
    #[ignore]
    fn test_real_glm_fetch() {
        let creds = GlmDiscoverer
            .discover()
            .into_iter()
            .next()
            .expect("应能发现 GLM 凭据（环境变量或 suppliers.json）");
        println!("凭据来源：{}", creds.source);
        let p = GlmProvider::new("acc-real", &creds.base, &creds.key).unwrap();
        match p.fetch().unwrap() {
            QuotaSnapshot::Windows(rows) => {
                assert_eq!(rows.len(), 2);
                for r in &rows {
                    let pct = r.used_percent.expect("TOKENS_LIMIT 应有百分比");
                    assert!((0.0..=100.0).contains(&pct), "百分比异常：{pct}");
                    assert!(r.reset_at.unwrap_or(0) > 1_700_000_000_000, "重置时间应为毫秒");
                    println!("[{}] 已用 {}% | 重置于 {}", r.window_kind, pct,
                        r.reset_at.map(|t| chrono::DateTime::from_timestamp_millis(t).map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()).unwrap_or_default());
                }
            }
            _ => panic!("GLM 应返回窗口口径快照"),
        }
    }
}
