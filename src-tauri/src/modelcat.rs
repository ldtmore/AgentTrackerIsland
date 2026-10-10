//! 模型目录：LiteLLM 单价表快照加载、模型→供应商归因、成本估算（M3-1，06-PLAN §6）。
//!
//! 职责边界：本模块只做「模型名 → 单价 / 厂商」的纯内存映射，不做任何 IO 与存储；
//! 用户单价覆盖表（model_price_overrides）的读取由 store 层完成，经 [`resolve_price`]
//! 在乘价处合并。快照随二进制编译分发（include_str!），更新流程见
//! resources/model_prices.REVISION.md。

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::store::TokenBreakdown;

/// 编译期嵌入的 LiteLLM 单价表快照（约 2.9MB，快照即版本、离线可用——ccusage 范本）
const PRICES_JSON: &str = include_str!("../resources/model_prices_and_context_window.json");

/// 快照 revision（与 resources/model_prices.REVISION.md 同步更新）
const PRICES_REVISION: &str = "ddc7ee6838726f2c329202bc4bef9abca58e1b35";

/// 单模型单价条目（美元 / token；LiteLLM 全表为 USD 计价）
#[derive(Debug, Clone)]
pub struct PriceEntry {
    pub input: f64,
    pub output: f64,
    /// 缓存读单价；快照缺失 = 该模型无此计费桶，成本计算按 0 计（三桶口径，06 §6.3）
    pub cache_read: Option<f64>,
    /// 缓存写单价；同上
    pub cache_creation: Option<f64>,
    /// LiteLLM 平台归类（归因第三级兜底的取数来源）
    pub litellm_provider: Option<String>,
}

/// 用户单价覆盖（store 层 model_price_overrides 表的行投影，此处只做纯转换不碰库）
#[derive(Debug, Clone)]
pub struct PriceOverride {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_creation: Option<f64>,
}

/// 内置前缀规则表（最长前缀优先，命中即停；新增厂商 = 在此加一行，禁止散落 if-else——宪法§三）。
/// 前缀匹配天然免疫 `[1m]` 等上下文后缀与大小写（比较前已小写化）。
const PREFIX_RULES: &[(&str, &str)] = &[
    // —— 6 字符及以上 ——
    ("deepseek", "deepseek"),
    ("chatgpt", "openai"),
    ("claude", "anthropic"),
    ("moonshot", "moonshot"),
    ("minimax", "minimax"),
    ("mistral", "mistral"),
    ("codestral", "mistral"),
    ("pixtral", "mistral"),
    ("ministral", "mistral"),
    ("hunyuan", "tencent"),
    ("doubao", "bytedance"),
    ("gemini", "google"),
    // —— 4 字符 ——
    ("glm", "glm"),
    ("kimi", "moonshot"),
    ("qwen", "qwen"),
    ("abab", "minimax"),
    ("ernie", "baidu"),
    ("grok", "xai"),
    // —— 3 字符 ——
    ("o1", "openai"),
    ("o3", "openai"),
    ("o4", "openai"),
    ("qwq", "qwen"),
    ("mimo", "xiaomi"),
];

/// 聚合/中转平台的键首段：尾段同名（同一模型上架多平台）的候选中，
/// 它们的报价通常高于官方价，排序时压到最后（仅影响取哪个候选单价，估算场景足够）
const AGGREGATOR_PLATFORMS: &[&str] = &[
    "openrouter",
    "vercel_ai_gateway",
    "aihubmix",
    "novita",
    "deepinfra",
    "fireworks_ai",
    "together_ai",
    "perplexity",
    "cloudflare",
    "fal_ai",
];

/// 单价目录：精确键 + 尾段两级索引。LiteLLM 顶层键多为「平台/模型」形式
/// （裸键仅约 609/4328，glm-5.3 / kimi-k2 等主流国产模型全靠尾段兜回）
struct Catalog {
    /// 裸键（小写）→ 条目
    exact: HashMap<String, PriceEntry>,
    /// 尾段（小写）→ 候选条目，构建期已按「非聚合平台 → 键段数少 → 字典序」稳定排序
    by_tail: HashMap<String, Vec<PriceEntry>>,
}

static CATALOG: OnceLock<Catalog> = OnceLock::new();

/// 进程内单例：首次成本计算/归因时解析快照（约 4 千条，毫秒级），此后零开销
fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| parse_catalog(PRICES_JSON))
}

/// 解析快照 JSON：宽容跳过无法提取单价的条目（非对象值、缺 input 单价、类型不符），
/// 解析失败时留空目录（全部模型显示缺单价「--」，不阻塞应用）
fn parse_catalog(json: &str) -> Catalog {
    let mut exact = HashMap::new();
    let mut by_tail: HashMap<String, Vec<(bool, usize, String, PriceEntry)>> = HashMap::new();
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        log::warn!("[模型目录] LiteLLM 快照解析失败，成本口径降级为全量缺价");
        return Catalog { exact: HashMap::new(), by_tail: HashMap::new() };
    };
    if let Some(map) = root.as_object() {
        for (key, val) in map {
            let Some(obj) = val.as_object() else { continue };
            let num = |k: &str| obj.get(k).and_then(|v| v.as_f64());
            // input 单价是四桶计价的最低要求；output 缺省 0（仅音频转写等特殊条目）
            let Some(input) = num("input_cost_per_token") else { continue };
            let entry = PriceEntry {
                input,
                output: num("output_cost_per_token").unwrap_or(0.0),
                cache_read: num("cache_read_input_token_cost"),
                cache_creation: num("cache_creation_input_token_cost"),
                litellm_provider: obj
                    .get("litellm_provider")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            };
            let lower = key.to_ascii_lowercase();
            let segs = lower.split('/').count();
            let tail = lower.rsplit('/').next().unwrap_or(&lower).to_string();
            let is_agg = AGGREGATOR_PLATFORMS.contains(&lower.split('/').next().unwrap_or(""));
            let is_bare = !lower.contains('/');
            if is_bare {
                exact.insert(key.to_ascii_lowercase(), entry.clone());
            }
            by_tail
                .entry(tail)
                .or_default()
                .push((is_agg, segs, lower, entry));
        }
    }
    // 尾段候选排序：官方/云平台价优先于聚合商报价；键越短越接近官方裸条目；最后按字典序稳定
    for cands in by_tail.values_mut() {
        cands.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    }
    let by_tail: HashMap<String, Vec<PriceEntry>> = by_tail
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().map(|x| x.3).collect()))
        .collect();
    log::debug!(
        "[模型目录] LiteLLM 快照加载完成（revision {PRICES_REVISION}，精确键 {} 条，尾段索引 {} 组）",
        exact.len(),
        by_tail.len()
    );
    Catalog { exact, by_tail }
}

/// 单价查找（裸模型名，调用方须已小写化）：裸键精确 → 尾段候选第一（构建期已排好序）
pub fn lookup_price(model_lower: &str) -> Option<&'static PriceEntry> {
    let cat = catalog();
    if let Some(e) = cat.exact.get(model_lower) {
        return Some(e);
    }
    cat.by_tail.get(model_lower).and_then(|v| v.first())
}

/// 单价解析（乘价处总入口）：用户覆盖表优先 → LiteLLM 快照；None = 缺单价（报表显示「--」）
pub fn resolve_price(
    model_lower: &str,
    overrides: &HashMap<String, PriceOverride>,
) -> Option<PriceEntry> {
    if let Some(o) = overrides.get(model_lower) {
        return Some(PriceEntry {
            input: o.input,
            output: o.output,
            cache_read: o.cache_read,
            cache_creation: o.cache_creation,
            litellm_provider: None, // 覆盖行只管价格，归因在乘价前已由模型名完成
        });
    }
    lookup_price(model_lower).cloned()
}

/// 模型名 → 供应商标识（06-PLAN §6.2，命中即停）：
/// ① 斜杠形式（OpenRouter/MiMo 风格）直取 vendor 段（小写）；
/// ② 内置前缀规则表（最长前缀优先）；
/// ③ LiteLLM 快照命中条目的 litellm_provider 兜底（常见平台名归一，其余原样小写）；
/// 全部未命中返回 None（报表按「未知供应商」聚合）
pub fn resolve_provider(model: &str) -> Option<String> {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return None;
    }
    if let Some((vendor, rest)) = m.split_once('/') {
        if !vendor.is_empty() && !rest.is_empty() {
            return Some(vendor.to_string());
        }
    }
    for (prefix, id) in PREFIX_RULES {
        if m.starts_with(prefix) {
            return Some((*id).to_string());
        }
    }
    lookup_price(&m)
        .and_then(|e| e.litellm_provider.clone())
        .map(|p| normalize_provider_id(&p))
}

/// LiteLLM 平台名 → 本项目供应商标识：仅归一「平台名即某厂商」的确定对应；
/// 多租户云平台（bedrock/azure 等）不指向单一厂商，原样返回（诚实分组）
fn normalize_provider_id(p: &str) -> String {
    let p = p.to_ascii_lowercase();
    match p.as_str() {
        "gemini" | "google" => "google".into(),
        "dashscope" | "qwen_ai_platform" | "qwencloud" | "aliyun" => "qwen".into(),
        "zhipu" => "glm".into(),
        other => other.to_string(),
    }
}

/// 四桶成本估算（美元）：input×input + output×output + 缓存读/写×对应单价；
/// 快照缺失的缓存桶按 0 计（06 §6.3「cc 字段缺失的模型按三桶计」）
pub fn cost_of(entry: &PriceEntry, b: &TokenBreakdown) -> f64 {
    b.input as f64 * entry.input
        + b.output as f64 * entry.output
        + b.cache_read as f64 * entry.cache_read.unwrap_or(0.0)
        + b.cache_creation as f64 * entry.cache_creation.unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_provider_prefix_rules() {
        // 前缀规则：大小写不敏感、免疫 [1m] 后缀
        assert_eq!(resolve_provider("GLM-5.3"), Some("glm".into()));
        assert_eq!(resolve_provider("glm-4.6[1m]"), Some("glm".into()));
        // 短名用例：claude-sonnet-4 的 LiteLLM 尾段候选是 gmi/claude-sonnet-4
        // （provider=gmi）——若前缀规则失效，兜底会错归 "gmi"，此断言即失败
        assert_eq!(resolve_provider("claude-sonnet-4"), Some("anthropic".into()));
        assert_eq!(resolve_provider("Claude-Sonnet-4-5"), Some("anthropic".into()));
        assert_eq!(resolve_provider("gpt-5.2"), Some("openai".into()));
        assert_eq!(resolve_provider("o4-mini"), Some("openai".into()));
        assert_eq!(resolve_provider("deepseek-chat"), Some("deepseek".into()));
        assert_eq!(resolve_provider("kimi-k2.5"), Some("moonshot".into()));
        assert_eq!(resolve_provider("moonshotai.kimi-k2.5"), Some("moonshot".into()));
        assert_eq!(resolve_provider("qwen3-coder-plus"), Some("qwen".into()));
        assert_eq!(resolve_provider("qwq-32b"), Some("qwen".into()));
        assert_eq!(resolve_provider("gemini-3-pro"), Some("google".into()));
        assert_eq!(resolve_provider("grok-4"), Some("xai".into()));
        assert_eq!(resolve_provider("minimax-m2"), Some("minimax".into()));
        assert_eq!(resolve_provider("mistral-large-3"), Some("mistral".into()));
        assert_eq!(resolve_provider("MiMo-v2.5"), Some("xiaomi".into()));
        // 前缀表与 LiteLLM 兜底都未命中 → None
        assert_eq!(resolve_provider("totally-unknown-model"), None);
        assert_eq!(resolve_provider(""), None);
    }

    #[test]
    fn test_resolve_provider_slash_form() {
        // 斜杠形式直取 vendor 段（小写），优先于前缀规则（06 §6.2 字面语义）
        assert_eq!(resolve_provider("openai/gpt-4o"), Some("openai".into()));
        assert_eq!(resolve_provider("zhipuai/GLM-4.6"), Some("zhipuai".into()));
        assert_eq!(resolve_provider("openrouter/xiaomi/mimo-v2.5"), Some("openrouter".into()));
        // 斜杠结尾等畸形输入不走斜杠分支，落回前缀规则
        assert_eq!(resolve_provider("glm-5.3/"), Some("glm".into()));
    }

    #[test]
    fn test_snapshot_parse_and_lookup() {
        // 真快照抽验：规模下限 + 已知模型可查且单价为正。
        // exact 只存裸键——LiteLLM 顶层键约 85% 为「平台/模型」形式（裸键约 600 条），
        // 其余全靠尾段索引兜回，故 exact 的合理量级是数百而非数千
        let cat = catalog();
        assert!(cat.exact.len() > 500, "裸键条目过少：{}", cat.exact.len());
        // 裸键精确：Anthropic/OpenAI/DeepSeek 官方条目
        let claude = lookup_price("claude-sonnet-4-5-20250929").expect("claude 条目缺失");
        assert!(claude.input > 0.0 && claude.output > claude.input);
        assert_eq!(claude.litellm_provider.as_deref(), Some("anthropic"));
        let gpt = lookup_price("gpt-4o").expect("gpt-4o 条目缺失");
        assert!(gpt.input > 0.0);
        let ds = lookup_price("deepseek-chat").expect("deepseek-chat 条目缺失");
        assert!(ds.input > 0.0 && ds.cache_read.is_some());
        // 尾段兜回：glm-5.3 无裸键，经 zai/glm-5.3 等候选兜回且非聚合平台优先
        let glm = lookup_price("glm-5.3").expect("glm-5.3 尾段兜回失败");
        assert!(glm.input > 0.0);
        let kimi = lookup_price("kimi-k2.5").expect("kimi-k2.5 尾段兜回失败");
        assert!(kimi.input > 0.0);
        // 归因第三级兜底：前缀规则未命中的模型经快照 litellm_provider 归因
        assert_eq!(resolve_provider("nvidia/llama-3.1-nemotron-70b-instruct").is_some(), true);
    }

    #[test]
    fn test_cost_of_matches_manual_calc() {
        // 手算一致性（验收硬要求）：sonnet 单价 × 指定 token 数
        let entry = PriceEntry {
            input: 0.000_003,
            output: 0.000_015,
            cache_read: Some(0.000_000_3),
            cache_creation: Some(0.000_003_75),
            litellm_provider: None,
        };
        let b = TokenBreakdown {
            input: 1_000_000,
            output: 500_000,
            cache_read: 2_000_000,
            cache_creation: 400_000,
        };
        // 1M×3e-6 + 0.5M×1.5e-5 + 2M×3e-7 + 0.4M×3.75e-6 = 3 + 7.5 + 0.6 + 1.5 = 12.6 美元
        let cost = cost_of(&entry, &b);
        assert!((cost - 12.6).abs() < 1e-9, "手算不一致：{cost}");
        // 缓存桶缺失 → 三桶口径（该桶计 0）
        let three_bucket = PriceEntry { cache_read: None, cache_creation: None, ..entry };
        let cost2 = cost_of(&three_bucket, &b);
        assert!((cost2 - (3.0 + 7.5)).abs() < 1e-9, "三桶口径不一致：{cost2}");
    }

    #[test]
    fn test_resolve_price_override_wins() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "my-private-model".to_string(),
            PriceOverride { input: 1.0, output: 2.0, cache_read: None, cache_creation: None },
        );
        // 覆盖表命中：直接采用，不查快照
        let p = resolve_price("my-private-model", &overrides).unwrap();
        assert_eq!((p.input, p.output), (1.0, 2.0));
        // 未命中覆盖表：落回快照
        let p = resolve_price("gpt-4o", &overrides).unwrap();
        assert!(p.input > 0.0 && p.input < 1.0);
        // 两级都未命中：None（报表显示 --）
        // 两级都未命中：None（报表显示 --）
        assert!(resolve_price("no-such-model-anywhere", &overrides).is_none());
    }
}
