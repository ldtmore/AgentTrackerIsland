//! Provider（供应商）适配器模块：额度查询抽象（M3-2 起多实例化）。
//! 两层数据模型（06-PLAN §2）：厂商定义（kinds() 内置静态注册表）＋用户实例
//! （store::ProviderAccount）；适配器由工厂按实例构建，每厂商一个文件。
//! 本模块是唯一允许的对外网络请求点（宪法§二红线①）。

pub mod bootstrap;
pub mod claude_local;
pub mod custom_openai;
pub mod deepseek;
pub mod glm;
pub mod moonshot;
pub mod openrouter;
pub mod siliconflow;
pub mod worker;
pub mod zhipu_balance;

use crate::store::{BalanceRow, ProviderAccount, QuotaRow};

/// 钥匙串 service 名（keyring 三元组之一；user = 实例 id）
pub const KEYRING_SERVICE: &str = "AgentTrackerIsland";

/// 各适配器共用的 HTTP 客户端构建：5s 超时（宪法§三：所有外呼必须有超时）；
/// 构建带超时的 HTTP 客户端（全适配器共用）。构建链：总超时 5s → 重试一次 →
/// 仅连接超时 5s → 仍失败返回 Err（本轮跳过该实例外呼，按退避重试）——绝不
/// 降级为无超时客户端（2026-09-29 审查修复：曾兜底 Client::new() 使外呼可能
/// 永久挂起，违反"所有外呼必须有超时"红线的最后口子；Err 透传装载留痕）
pub(crate) fn http_client(label: &str) -> anyhow::Result<reqwest::blocking::Client> {
    let full = || {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
    };
    full()
        .or_else(|_| full())
        .or_else(|_| {
            reqwest::blocking::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
        })
        .map_err(|e| anyhow::anyhow!("[额度] {label} HTTP 客户端构建失败（本轮跳过外呼，稍后重试）：{e}"))
}

/// 额度口径（厂商定义声明，UI 与存储按口径分流渲染）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaKind {
    /// 订阅窗口（5h/周/月进度条）
    Windows,
    /// 货币余额（金额大字＋拆分行）
    Balance,
    /// 本地推算（M3-12，Claude Pro/Max 窗口估算）
    LocalEstimate,
}

/// 额度快照统一枚举：两种口径，UI 与存储按口径分流（06-PLAN §2.2）
#[derive(Debug)]
pub enum QuotaSnapshot {
    /// 订阅窗口口径（可多条：5h/周/月）
    Windows(Vec<QuotaRow>),
    /// 货币余额口径
    Balance(BalanceRow),
}

/// 站点变体（2026-10-10 双站机制定稿）：同厂商国内/国际站的静态预设——双站
/// 接口全部同构仅域名与账号体系不同（调研确证：智谱/Moonshot/硅基流动），
/// 一套适配器换 base 即可，故只做数据不做新 kind；「变体=接口同构」约束下
/// base_override 可反推变体，无需给 provider_accounts 加 region 列。首例
/// 接口不同构的（如 Kimi Code 会员线 api.kimi.com）应建模为独立 kind
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderVariant {
    /// 变体键："cn" | "intl"（kind 内唯一；数组首项=默认变体，空 base 覆盖归它）
    pub key: &'static str,
    /// 站别徽标文案（选择器条目行尾；双站厂商成对戴、单站无——防止
    /// 「没有徽标=国内站还是不分站」的隐性推断）
    pub badge: &'static str,
    /// 运行时显示名（岛/托盘/额度页 kind_name 位投影）：官方品牌即最精确
    /// 表达，如国际站用 "Z.ai" 而非 "智谱 GLM 国际站"
    pub display_name: &'static str,
    /// 该站默认端点（选中变体即显式回填 base_override；空覆盖语义仍归 kind）
    pub default_base: &'static str,
    /// 新建实例默认别名（选择即带出，用户可改）
    pub default_alias: &'static str,
    /// 副行俗名（选择器条目「俗名 · 域名」的俗名位；空=该条目不显俗名）
    pub alt_name: &'static str,
    /// 该站余额币种（Balance 口径变体消费：Moonshot/硅基流动国际站余额为
    /// USD，硬编码 CNY 会「¥ 符号装美元数值」——build_adapter 按变体传入）
    pub currency: &'static str,
    /// 站点级凭据指引（覆盖 kind 级；关键差异前置告知，如 Moonshot 国际站
    /// 的双站 key 互不通用混用 401）
    pub cred_hint: &'static str,
}

/// 厂商定义描述符（内置静态注册表；新增厂商 = 实现适配器＋此处加一行，
/// 禁止散落 if-else——宪法§三）
pub struct ProviderKindDef {
    /// 厂商 id：'glm' | 'deepseek' | ...
    pub id: &'static str,
    /// 显示名（2026-10-10 定名表：名从主人——官方在中文开发者语境的主打名，
    /// 不互译不并置；公司名/俗名降入 alt_name 与 aliases，永不上屏主名）
    pub name: &'static str,
    /// 品牌色（岛点状指示器/卡片色条，M3-6 消费）
    pub color: &'static str,
    /// 默认端点，实例可覆盖（=默认变体端点，空 base 覆盖语义不变）
    pub default_base: &'static str,
    /// 额度口径
    pub quota_kind: QuotaKind,
    /// Balance 口径的币种提示（无变体厂商消费；有变体按 variant.currency）
    pub currency: &'static str,
    /// 凭据获取指引（设置页 ⓘ 悬浮，M3-4 消费；有变体厂商被变体级覆盖）
    pub cred_hint: &'static str,
    /// 本地推算厂商（M3-12 claude-local）：无凭据、无端点、无外呼——
    /// resolve_creds 跳过凭据解析、create/update 跳过凭据必填、表单隐藏
    /// 凭据与接口地址行
    pub local_only: bool,
    /// 站点变体表（空切片=单站厂商：选择器不展开、不戴徽标、币种取 kind 级）
    pub variants: &'static [ProviderVariant],
    /// 搜索别名（永不上屏，仅参与选择器匹配；显示名/域名天然参与无需重复）
    pub aliases: &'static [&'static str],
    /// 副行俗名（选择器条目「俗名 · 域名」的俗名位；官方中文俗名有官方出处
    /// 才填，如 DeepSeek↔深度求索（官网标题并称）；空=不显俗名）
    pub alt_name: &'static str,
}

/// 智谱 GLM（2026-10-10 定名：GLM=模型系/开放平台产品名，与 Z.ai 同层对称；
/// 公司名「智谱」降入 alt_name 与搜索别名——官方自称「智谱（Z.AI）」，
/// zhipuai.cn「关于智谱」页）
const GLM_DEF: ProviderKindDef = ProviderKindDef {
    id: "glm",
    name: "GLM",
    color: "#2f6bff",
    default_base: glm::BASE_BIGMODEL,
    quota_kind: QuotaKind::Windows,
    currency: "CNY",
    cred_hint: "智谱开放平台 API Key（与 Claude Code 的 ANTHROPIC_AUTH_TOKEN 同值）",
    local_only: false,
    // 双站接口同构（/api/monitor/usage/quota/limit 两站一致，域名不同），
    // 一套适配器换 base 即可；国际站官方品牌 Z.AI（z.ai，智谱中外双品牌形态）
    variants: &[
        ProviderVariant {
            key: "cn",
            badge: "国内站",
            display_name: "GLM",
            default_base: glm::BASE_BIGMODEL,
            default_alias: "GLM",
            alt_name: "智谱",
            currency: "CNY",
            cred_hint: "智谱开放平台 API Key（open.bigmodel.cn 控制台创建；与 Claude Code 的 ANTHROPIC_AUTH_TOKEN 同值）",
        },
        ProviderVariant {
            key: "intl",
            badge: "国际站",
            display_name: "Z.ai",
            default_base: glm::BASE_ZAI,
            default_alias: "Z.ai",
            alt_name: "智谱国际站",
            currency: "CNY",
            cred_hint: "智谱国际站（Z.AI Open Platform）API Key（z.ai 控制台创建；国内/国际站账号体系独立，key 互不通用）",
        },
    ],
    aliases: &["zhipu", "智谱", "bigmodel", "chatglm", "zai", "z.ai"],
    alt_name: "智谱",
};

/// DeepSeek 开放平台（M3-3，Balance 口径）：单平台无国际站（deepseek.com
/// 中/EN 切换）；「深度求索」为官方并用名（官网标题「DeepSeek | 深度求索」）
const DEEPSEEK_DEF: ProviderKindDef = ProviderKindDef {
    id: "deepseek",
    name: "DeepSeek",
    color: "#4d6bfe",
    default_base: deepseek::BASE_DEEPSEEK,
    quota_kind: QuotaKind::Balance,
    currency: "CNY",
    cred_hint: "DeepSeek 开放平台 API Key（platform.deepseek.com 左侧 API Keys 页创建）",
    local_only: false,
    variants: &[],
    aliases: &["ds", "深度求索"],
    alt_name: "深度求索",
};

/// Moonshot（Kimi）开放平台（M3-3，Balance 口径）：2026-10-10 定名「Kimi」
/// （官方主打名——开发者平台已改名 platform.kimi.com「Kimi API 开放平台」；
/// 公司名「月之暗面」降入 alt_name，俗称「月暗」入搜索别名）。双站 key 完全
/// 独立（官方文档：混用 401），国际站 api.moonshot.ai 无独立品牌名，按方案 A
/// 定名「Kimi 国际站」（主品牌＋限定词；官方无双品牌可用，不编造裸名）
const MOONSHOT_DEF: ProviderKindDef = ProviderKindDef {
    id: "moonshot",
    name: "Kimi",
    // 官方品牌近黑（#18181b）在深色 UI 的 chip 选中态不可辨识（M3-4 实测），
    // 改用"月之暗面"意象的暖金作 UI 展示色——四家色板唯一暖色，双主题可辨
    color: "#fbbf24",
    default_base: moonshot::BASE_MOONSHOT,
    quota_kind: QuotaKind::Balance,
    currency: "CNY",
    cred_hint: "Kimi 开放平台 API Key（platform.kimi.com 创建）",
    local_only: false,
    variants: &[
        ProviderVariant {
            key: "cn",
            badge: "国内站",
            display_name: "Kimi",
            default_base: moonshot::BASE_MOONSHOT,
            default_alias: "Kimi",
            alt_name: "月之暗面",
            currency: "CNY",
            cred_hint: "Kimi 开放平台 API Key（platform.kimi.com 创建）",
        },
        ProviderVariant {
            key: "intl",
            badge: "国际站",
            display_name: "Kimi 国际站",
            default_base: moonshot::BASE_MOONSHOT_INTL,
            default_alias: "Kimi 国际站",
            alt_name: "Moonshot AI",
            currency: "USD",
            cred_hint: "Moonshot 国际站 API Key（platform.kimi.ai 创建；国际站与国内站 key 互不通用，混用返回 401）",
        },
    ],
    aliases: &["moonshot", "月之暗面", "月暗", "moonshotai"],
    alt_name: "月之暗面",
};

/// 硅基流动（M3-3，Balance 口径）：官方中文品牌（siliconflow.cn about 页
/// 自称「硅基流动（SiliconFlow）」）。注意国内平台产品名是 SiliconCloud
/// （cloud.siliconflow.cn）勿与公司/国际站品牌 SiliconFlow 混用（2026-10-10
/// 名称专项核实）；国际站 api.siliconflow.com 账号独立、余额币种 USD
const SILICONFLOW_DEF: ProviderKindDef = ProviderKindDef {
    id: "siliconflow",
    name: "硅基流动",
    color: "#6b7cff",
    default_base: siliconflow::BASE_SILICONFLOW,
    quota_kind: QuotaKind::Balance,
    currency: "CNY",
    cred_hint: "硅基流动 API Key（cloud.siliconflow.cn 账户页「API 密钥」创建）",
    local_only: false,
    variants: &[
        ProviderVariant {
            key: "cn",
            badge: "国内站",
            display_name: "硅基流动",
            default_base: siliconflow::BASE_SILICONFLOW,
            default_alias: "硅基流动",
            alt_name: "SiliconCloud",
            currency: "CNY",
            cred_hint: "硅基流动 API Key（cloud.siliconflow.cn 账户页「API 密钥」创建）",
        },
        ProviderVariant {
            key: "intl",
            badge: "国际站",
            display_name: "SiliconFlow",
            default_base: siliconflow::BASE_SILICONFLOW_INTL,
            default_alias: "SiliconFlow",
            alt_name: "SiliconFlow AI Cloud",
            currency: "USD",
            cred_hint: "SiliconFlow 国际站 API Key（cloud.siliconflow.com 创建；国际站与国内站账号独立，余额币种 USD）",
        },
    ],
    aliases: &["siliconflow", "siliconcloud", "硅基", "sf"],
    alt_name: "SiliconCloud",
};

/// OpenRouter（M3-7，Balance/USD 口径）：统一路由多家模型的中转平台。
/// credits/activity 端点要求 Management Key（普通推理 Key 返回 403），
/// 官方建议设过期时间（过期后查询 401，适配器文案引导重建）
const OPENROUTER_DEF: ProviderKindDef = ProviderKindDef {
    id: "openrouter",
    name: "OpenRouter",
    // 官方站点主题近黑（rgb(9,10,11)）无独立可辨品牌色，沿用 Moonshot 先例取
    // 意象色：紫（"路由/汇聚"意象），与硅基流动的蓝紫拉开色相差，双主题可辨
    color: "#8b5cf6",
    default_base: openrouter::BASE_OPENROUTER,
    quota_kind: QuotaKind::Balance,
    currency: "USD",
    cred_hint: "OpenRouter Management Key（openrouter.ai/settings/management-keys 页「Create New Key」创建；普通推理 Key 无余额查询权限；官方建议设过期时间，到期前需重建更换）",
    local_only: false,
    variants: &[],
    aliases: &[],
    alt_name: "",
};

/// 智谱开放平台按量账户（M3-8，Balance/CNY 口径）：与 GLM_DEF 同一平台的
/// 另一种账户视角——按量余额随调用扣减，Coding Plan 订阅窗口互不影响，
/// 建议两个实例并存分别监控。端点为未文档化内部接口（best-effort），
/// cred_hint 注明来源不稳定。2026-10-10 随定名表改「GLM 按量」（与 GLM
/// 保持家族感；公司名「智谱」在副行俗名位）
const ZHIPU_BALANCE_DEF: ProviderKindDef = ProviderKindDef {
    id: "zhipu-balance",
    name: "GLM 按量",
    // 与 GLM 同厂商异口径：色相向青绿偏移区分（同厂商两 kind 在岛指示器
    // 并存时不可混淆——instanceColor 只压暗同 kind 组内）
    color: "#14b8a6",
    default_base: zhipu_balance::BASE_ZHIPU_BALANCE,
    quota_kind: QuotaKind::Balance,
    currency: "CNY",
    cred_hint: "智谱开放平台 API Key（bigmodel.cn 控制台创建，仅国内站；按量余额随调用扣减，与 Coding Plan 订阅窗口互不影响。查询走控制台内部接口，无官方文档、随时可能变更）",
    local_only: false,
    variants: &[],
    aliases: &["zhipu", "智谱", "按量"],
    alt_name: "智谱",
};

/// 自定义 OpenAI 兼容·中转站（M3-10，Balance/USD 口径）：one-api 系 billing
/// 兼容接口。站点各异故 base 必填（注册表无默认端点，create/update 命令
/// 与工厂三重校验）；额度口径随站点配置（cred_hint ⓘ）
const CUSTOM_OPENAI_DEF: ProviderKindDef = ProviderKindDef {
    id: "custom-openai",
    name: "自定义中转",
    // 自定义站点无官方品牌色，取暖橙与既有六家（四蓝一紫一青一金）全区分
    color: "#f97316",
    default_base: custom_openai::BASE_CUSTOM_OPENAI,
    quota_kind: QuotaKind::Balance,
    currency: "USD",
    cred_hint: "中转站 API Key（one-api 系 billing 兼容接口）；接口地址（base）必填，填站点根（如 https://relay.example.com）；额度口径随站点配置——字段名为 USD，未开启货币显示的站点装的是内部点数",
    local_only: false,
    variants: &[],
    aliases: &["oneapi", "newapi", "中转", "relay", "openai"],
    alt_name: "OpenAI 兼容",
};

/// Claude Pro/Max 订阅本地推算（M3-12，LocalEstimate 口径）：官方不暴露
/// 用量指标，本地聚合 Claude Code 用量按档位限额推算窗口消耗——无凭据、
/// 无端点、无外呼（local_only 全套特化）
const CLAUDE_LOCAL_DEF: ProviderKindDef = ProviderKindDef {
    id: "claude-local",
    name: "Claude 订阅",
    // 翠绿：色板唯一绿，与既有七家（四蓝一紫一青一金一橙）全区分
    color: "#10b981",
    default_base: "",
    quota_kind: QuotaKind::LocalEstimate,
    currency: "CNY",
    cred_hint: "无需凭据：本地聚合 Claude Code 用量推算订阅窗口消耗（5h 限额为社区测算值；仅统计 Claude Code 用量；7 天合计无限额仅展示）",
    local_only: true,
    variants: &[],
    aliases: &["claude", "cc"],
    alt_name: "",
};

/// 内置厂商注册表（静态清单，新增厂商在此登记）。顺序即选择器条目顺序：
/// 同厂商的 kind 紧邻声明（GLM 家族三连：国内/国际/按量），变体按声明序
/// 在各 kind 内展开——用户在列表里看到「同一家的条目排在一起」
pub fn kinds() -> &'static [ProviderKindDef] {
    &[
        GLM_DEF,
        ZHIPU_BALANCE_DEF,
        DEEPSEEK_DEF,
        MOONSHOT_DEF,
        SILICONFLOW_DEF,
        OPENROUTER_DEF,
        CUSTOM_OPENAI_DEF,
        CLAUDE_LOCAL_DEF,
    ]
}

/// 按厂商 id 查定义
pub fn kind_of(kind_id: &str) -> Option<&'static ProviderKindDef> {
    kinds().iter().find(|k| k.id == kind_id)
}

/// URL 取 host（去 scheme 与路径；解析不出返回原文）——选择器条目副行与
/// glm.rs 发现器共用（std 手写，不为此引 url crate）
pub(crate) fn host_of(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
}

/// base 匹配归一：trim＋去尾斜杠＋小写（host 不区分大小写、尾斜杠是常见
/// 粘贴形态）——变体反推按此口径，避免 `https://api.z.ai/` 误判为自定义地址
fn norm_base(s: &str) -> String {
    s.trim().trim_end_matches('/').to_lowercase()
}

/// 按 base 覆盖匹配站点变体：空覆盖=默认变体（数组首项）；归一化后与某
/// 变体 default_base 相等=该变体；其余（自定义域名/中转）=None——身份无法
/// 确证时回退 kind 级，不强行归组（编辑态同口径）
pub fn variant_of(
    kind_id: &str,
    base_override: Option<&str>,
) -> Option<&'static ProviderVariant> {
    let k = kind_of(kind_id)?;
    if k.variants.is_empty() {
        return None;
    }
    match base_override.map(str::trim).filter(|s| !s.is_empty()) {
        None => k.variants.first(),
        Some(b) => {
            let target = norm_base(b);
            k.variants.iter().find(|v| norm_base(v.default_base) == target)
        }
    }
}

/// 实例的运行时厂商显示名（岛/托盘/报表 kind_name 位投影）：命中变体取
/// display_name（如 Z.ai 实例不再显示「GLM」），未命中回退 kind.name，
/// 注册表缺该厂商兜底 kind_id（与既有 unknown-kind 口径一致）
pub fn runtime_kind_name(kind_id: &str, base_override: Option<&str>) -> String {
    match kind_of(kind_id) {
        None => kind_id.to_string(),
        Some(k) => variant_of(kind_id, base_override)
            .map(|v| v.display_name.to_string())
            .unwrap_or_else(|| k.name.to_string()),
    }
}

/// 实例的余额币种（Balance 口径双站厂商消费）：命中变体取 variant.currency
/// （Moonshot/硅基流动国际站为 USD），未命中/无变体回退 kind 级
pub(crate) fn instance_currency(account: &crate::store::ProviderAccount) -> &'static str {
    let kind_currency = kind_of(&account.kind_id).map(|k| k.currency).unwrap_or("CNY");
    variant_of(&account.kind_id, account.base_override.as_deref())
        .map(|v| v.currency)
        .unwrap_or(kind_currency)
}

/// API Key 脱敏显示（2026-10-10 所有者定稿三档规则，1Password/GitHub 同款形态）：
/// ≥16 位：前 8 位＋…＋尾 4 位（sk-ant-a…3f4a，前缀带出厂商辨识）；
/// 11～15 位：前 3 位＋…＋尾 4 位；<11 位：整体打码——太短的 key 尾部
/// 可辨识度不足以抵消泄露面，宁可不显示。真值永不因此出后端
pub(crate) fn mask_key(key: &str) -> String {
    let n = key.chars().count();
    if n >= 16 {
        let head: String = key.chars().take(8).collect();
        let tail: String = key.chars().skip(n - 4).collect();
        format!("{head}…{tail}")
    } else if n >= 11 {
        let head: String = key.chars().take(3).collect();
        let tail: String = key.chars().skip(n - 4).collect();
        format!("{head}…{tail}")
    } else {
        "••••••••".into()
    }
}

/// 工厂层已解析的明文凭据：解析（解 env 引用/取钥匙串）在工厂层完成后
/// 以明文传入，适配器不感知存储细节（06-PLAN §2.2）
pub struct AccountCreds {
    /// API origin（厂商默认端点或实例覆盖）
    pub base: String,
    /// 明文 key
    pub key: String,
}

/// 凭据解析失败分类（06-PLAN §4.2；network_error 属查询层错误，不在此列）。
/// "凭据引用失效"与"网络失败"是两种不同的用户动作，必须分类留痕不得合并
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredState {
    /// plain 模式：钥匙串读取失败（凭据丢失）
    KeyringLost,
    /// env 模式：变量进程内未设置且注册表用户级也没有（凭据引用失效）
    EnvNotFound,
    /// env 模式：变量仅在注册表用户级命中、进程未继承（重启应用后生效，D13）
    EnvNeedsRestart,
}

impl CredState {
    /// 人类可读描述（日志/装载留痕用全语义版）
    pub fn describe(&self) -> String {
        match self {
            CredState::KeyringLost => "凭据丢失（钥匙串读取失败，请点该实例的「编辑」重新保存 Key，或改用环境变量模式）".into(),
            CredState::EnvNotFound => "凭据引用失效（环境变量未设置）".into(),
            // 终端会话临时变量对常驻进程无效（D13），须重启应用继承用户级变量
            CredState::EnvNeedsRestart => "环境变量已设置（注册表用户级）但本进程未继承，重启应用后生效".into(),
        }
    }

    /// 卡片等 UI 场景的精简文案（动作导向，一行内；原因细节在日志的 describe 全版
    /// 与 keyring 原始错误里）
    pub fn describe_short(&self) -> String {
        match self {
            CredState::KeyringLost => "凭据丢失，点「编辑」重新保存 Key 或改用环境变量模式".into(),
            CredState::EnvNotFound => "环境变量未设置，请检查变量名或改用明文 Key".into(),
            CredState::EnvNeedsRestart => "变量已设置但进程未继承，重启应用后生效".into(),
        }
    }
}

/// 供应商适配器：拉取单个实例的额度快照
pub trait ProviderAdapter: Send + Sync {
    /// 厂商标识（kind_id）：'glm' | 'deepseek' | ...
    fn id(&self) -> &'static str;

    /// 实例归属（快照入库 account_id）
    fn account_id(&self) -> &str;

    /// 拉取额度快照（实现必须自带超时，失败返回 Err 由调度器退避留痕，
    /// 展示降级为最近快照——红线④）
    fn fetch(&self) -> anyhow::Result<QuotaSnapshot>;

    /// 连接测试（设置页"检测"按钮，M3-4 消费）：轻量调用，返回人类可读摘要。
    /// 默认实现 = fetch 后走统一摘要格式化；特殊厂商（如 OpenRouter 附带
    /// activity）可覆盖
    fn test(&self) -> anyhow::Result<String> {
        Ok(describe_snapshot(&self.fetch()?))
    }
}

/// 快照 → 人类可读摘要（检测按钮/悬浮提示共用一份口径）
pub fn describe_snapshot(snap: &QuotaSnapshot) -> String {
    match snap {
        QuotaSnapshot::Windows(rows) => rows
            .iter()
            .map(|r| {
                let pct = r
                    .used_percent
                    .map(|p| format!("已用 {:.0}%", p))
                    .unwrap_or_else(|| "已用 --".into());
                // 窗口展示短标签（5h/7d），括号补中文全称——检测摘要是单行无悬浮，
                // 一次呈现全语义（与前端 QuotaChip"短标签＋悬浮全称"同口径；
                // 原始存储值 weekly 不出面向用户，文案口径 2026-09-25 统一）
                let (label, full) = match r.window_kind.as_str() {
                    "5h" => ("5h", "5 小时"),
                    "weekly" => ("7d", "7 天"),
                    other => (other, other),
                };
                if label == full {
                    format!("{label} {pct}")
                } else {
                    format!("{label}（{full}）{pct}")
                }
            })
            .collect::<Vec<_>>()
            .join("，"),
        QuotaSnapshot::Balance(b) => {
            let symbol = if b.currency == "USD" { "$" } else { "¥" };
            format!("余额 {symbol}{:.2}", b.total)
        }
    }
}

/// 工厂：kind 分派只在此一处（宪法"新增供应商＝实现 trait"的落地形态）。
/// 调用方保证 kind_id 已在 kinds() 注册且凭据已解析成功
/// 工厂：kind 分派只在此一处（宪法"新增供应商＝实现 trait"的落地形态）。
/// 调用方保证 kind_id 已在 kinds() 注册且凭据已解析成功；store 供本地推算
/// 厂商（claude-local）做聚合查询
pub fn build_adapter(
    account: &ProviderAccount,
    creds: &AccountCreds,
    store: std::sync::Arc<crate::store::Store>,
) -> anyhow::Result<Box<dyn ProviderAdapter>> {
    let local_only = kind_of(&account.kind_id).map(|k| k.local_only).unwrap_or(false);
    // 通用防御：default_base 为空的厂商（custom-openai）无 override 时 base 为空，
    // URL 会拼成相对路径——在此统一拦截，错误透传装载留痕/表单红字
    // （local_only 厂商无端点概念，不受此限）
    if creds.base.trim().is_empty() && !local_only {
        anyhow::bail!("实例缺少接口地址（base）：该厂商必须填写站点地址");
    }
    match account.kind_id.as_str() {
        "glm" => Ok(Box::new(glm::GlmProvider::new(
            &account.id,
            &creds.base,
            &creds.key,
        )?)),
        "deepseek" => Ok(Box::new(deepseek::DeepseekProvider::new(
            &account.id,
            &creds.base,
            &creds.key,
        )?)),
        // 双站厂商（变体机制，2026-10-10）：余额币种随站别——国际站余额为
        // USD，硬编码 CNY 会「¥ 符号装美元数值」；未命中变体（自定义域名）
        // 回退 kind 级币种
        "moonshot" => {
            let currency = instance_currency(&account);
            Ok(Box::new(moonshot::MoonshotProvider::new(
                &account.id,
                &creds.base,
                &creds.key,
                currency,
            )?))
        }
        "siliconflow" => {
            let currency = instance_currency(&account);
            Ok(Box::new(siliconflow::SiliconflowProvider::new(
                &account.id,
                &creds.base,
                &creds.key,
                currency,
            )?))
        }
        "openrouter" => Ok(Box::new(openrouter::OpenrouterProvider::new(
            &account.id,
            &creds.base,
            &creds.key,
        )?)),
        "zhipu-balance" => Ok(Box::new(zhipu_balance::ZhipuBalanceProvider::new(
            &account.id,
            &creds.base,
            &creds.key,
        )?)),
        "custom-openai" => Ok(Box::new(custom_openai::CustomOpenaiProvider::new(
            &account.id,
            &creds.base,
            &creds.key,
        )?)),
        "claude-local" => Ok(Box::new(claude_local::ClaudeLocalProvider::new(
            &account.id,
            store,
        ))),
        other => anyhow::bail!("未知厂商 kind：{other}（未注册适配器）"),
    }
}

/// 凭据解析（工厂层完成）：plain → keyring 读取；env → 进程环境变量 →
/// 注册表 HKCU\Environment 两层检测（D13）。失败按 CredState 分类返回
pub fn resolve_creds(account: &ProviderAccount) -> Result<AccountCreds, CredState> {
    let kind = kind_of(&account.kind_id);
    // 本地推算厂商（M3-12 claude-local）：无凭据无端点，直接空 creds 返回
    if kind.map(|k| k.local_only).unwrap_or(false) {
        return Ok(AccountCreds {
            base: String::new(),
            key: String::new(),
        });
    }
    let base = account
        .base_override
        .clone()
        .or_else(|| kind.map(|k| k.default_base.to_string()))
        .unwrap_or_default();
    let key = match account.cred_kind.as_str() {
        "env" => {
            let var = account.cred_value.as_deref().unwrap_or_default();
            if var.is_empty() {
                return Err(CredState::EnvNotFound);
            }
            env_key_lookup(var)?
        }
        // plain（含未知值兜底按 plain 处理）：真值在钥匙串，user = 实例 id。
        // keyring 真实错误必须留痕（2026-09-25 教训：keyring 3 未启用平台后端时
        // 静默落内存 mock，写入"成功"读取必失败，错误被吞导致全员凭据丢失无迹可查）
        _ => keyring::Entry::new(KEYRING_SERVICE, &account.id)
            .and_then(|e| e.get_password())
            .map_err(|e| {
                log::warn!(
                    "[凭据] 实例 {}（{}）钥匙串读取失败：{e}",
                    account.alias,
                    account.id
                );
                CredState::KeyringLost
            })?,
    };
    if key.is_empty() {
        // 钥匙串条目存在但值为空等同丢失（保存侧不允许空 key，防御性兜底）
        return Err(CredState::KeyringLost);
    }
    Ok(AccountCreds { base, key })
}

/// env 变量两层检测（D13）：先查当前进程 env；未命中查注册表用户级
/// HKCU\Environment——命中但进程未继承时返回 EnvNeedsRestart（提示"重启应用
/// 后生效"），终端会话临时变量对常驻进程无效故不直接采用注册表值。
/// pub(crate)：M3-4 设置页 env 模式"就地解析检测"命令复用同一份口径
pub(crate) fn env_key_lookup(var: &str) -> Result<String, CredState> {
    if let Ok(v) = std::env::var(var) {
        if !v.trim().is_empty() {
            return Ok(v);
        }
    }
    if let Ok(hkcu) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey("Environment")
    {
        if let Ok(v) = hkcu.get_value::<String, _>(var) {
            if !v.trim().is_empty() {
                return Err(CredState::EnvNeedsRestart);
            }
        }
    }
    Err(CredState::EnvNotFound)
}

/// 凭据发现结果：来自本机环境（环境变量/配置文件），只当建议者（06-PLAN §4.3）
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredCreds {
    pub base: String,
    pub key: String,
    /// 来源描述（写入 discovered 实例 note，设置页"来源"徽标承接旧 glm_token_source）
    pub source: &'static str,
}

/// 凭据发现器 trait：从本机环境发现某厂商凭据。启动时对零实例厂商尝试，
/// 命中则创建 origin='discovered' 实例；一切以实例表为准
pub trait ProviderDiscoverer: Send + Sync {
    fn kind_id(&self) -> &'static str;
    /// 返回全部命中（当前各发现器至多一条；Vec 为多账号发现留位）
    fn discover(&self) -> Vec<DiscoveredCreds>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 双站变体注册表完整性（2026-10-10 定稿数据）：三家双站厂商必须成对含
    /// cn+intl、首项=默认变体（cn）、端点为合法 https、关键字段非空——
    /// 注册表是选择器/运行时投影/币种的唯一真值源，数据破形即全链路错乱
    #[test]
    fn test_variant_registry_integrity() {
        let dual = ["glm", "moonshot", "siliconflow"];
        let single = ["deepseek", "openrouter", "zhipu-balance", "custom-openai", "claude-local"];
        for id in dual {
            let k = kind_of(id).unwrap_or_else(|| panic!("{id} 应在注册表"));
            assert_eq!(k.variants.len(), 2, "{id} 应含双站变体");
            assert_eq!(k.variants[0].key, "cn", "{id} 首项应为默认变体 cn");
            assert_eq!(k.variants[1].key, "intl", "{id} 次项应为 intl");
            for v in k.variants {
                assert!(v.default_base.starts_with("https://"), "{id}/{} 端点应为 https", v.key);
                assert!(v.default_base.contains(host_of(v.default_base)), "{id}/{} 端点应含 host", v.key);
                assert!(!v.badge.is_empty(), "{id}/{} 徽标非空", v.key);
                assert!(!v.display_name.is_empty(), "{id}/{} 显示名非空", v.key);
                assert!(!v.default_alias.is_empty(), "{id}/{} 默认别名非空", v.key);
                assert!(!v.cred_hint.is_empty(), "{id}/{} 凭据指引非空", v.key);
                assert!(!v.currency.is_empty(), "{id}/{} 币种非空", v.key);
            }
            // 双站厂商徽标必须成对（对称显式：只给国际戴会制造隐性推断）
            assert_ne!(k.variants[0].badge, k.variants[1].badge, "{id} 徽标应区分站别");
        }
        for id in single {
            let k = kind_of(id).unwrap_or_else(|| panic!("{id} 应在注册表"));
            assert!(k.variants.is_empty(), "{id} 应为单站厂商（无变体）");
        }
    }

    /// 变体匹配：空覆盖=默认变体；端点命中归一（尾斜杠/大小写）；自定义
    /// 域名与无变体厂商返回 None 不强行归组
    #[test]
    fn test_variant_of() {
        // 空覆盖 → 默认变体（国内站）
        assert_eq!(variant_of("glm", None).map(|v| v.key), Some("cn"));
        assert_eq!(variant_of("glm", Some("")).map(|v| v.key), Some("cn"));
        // 国际站端点命中（含尾斜杠与大小写归一）
        assert_eq!(variant_of("glm", Some("https://api.z.ai")).map(|v| v.key), Some("intl"));
        assert_eq!(variant_of("glm", Some("https://api.z.ai/")).map(|v| v.key), Some("intl"));
        assert_eq!(variant_of("glm", Some("HTTPS://API.Z.AI")).map(|v| v.key), Some("intl"));
        // 自定义域名（自建反代）→ None：身份无法确证不归组
        assert_eq!(variant_of("glm", Some("https://glm-proxy.example.com")), None);
        // 无变体厂商恒 None
        assert_eq!(variant_of("deepseek", Some("https://api.deepseek.com")), None);
        // 未知厂商 None
        assert_eq!(variant_of("no-such-kind", None), None);
    }

    /// 运行时定名投影：命中变体取 display_name，未命中回退 kind.name，
    /// 未知厂商兜底 kind_id（岛/托盘/报表/设置页四处同源）
    #[test]
    fn test_runtime_kind_name() {
        // Z.ai 实例不再显示「GLM」（双站变体机制的核心诉求）
        assert_eq!(runtime_kind_name("glm", None), "GLM");
        assert_eq!(runtime_kind_name("glm", Some("https://api.z.ai")), "Z.ai");
        // Kimi 双站（方案 A：Kimi / Kimi 国际站）
        assert_eq!(runtime_kind_name("moonshot", Some("https://api.moonshot.cn")), "Kimi");
        assert_eq!(
            runtime_kind_name("moonshot", Some("https://api.moonshot.ai/")),
            "Kimi 国际站"
        );
        // 自定义域名回退 kind 名；单站厂商恒 kind 名；未知兜底 kind_id
        assert_eq!(runtime_kind_name("glm", Some("https://x.example.com")), "GLM");
        assert_eq!(runtime_kind_name("deepseek", None), "DeepSeek");
        assert_eq!(runtime_kind_name("no-such-kind", None), "no-such-kind");
    }

    /// 双站币种不变量：Balance 口径厂商的国际站变体必须是 USD——硬编码 CNY
    /// 会「¥ 符号装美元数值」（Moonshot/硅基流动国际站余额为美元）
    #[test]
    fn test_intl_balance_variants_are_usd() {
        for id in ["moonshot", "siliconflow"] {
            let k = kind_of(id).unwrap();
            let intl = k.variants.iter().find(|v| v.key == "intl").unwrap();
            assert_eq!(intl.currency, "USD", "{id} 国际站余额币种应为 USD");
        }
        // Windows 口径厂商（GLM 订阅窗口显示百分比）币种无消费方，不约束
    }

    /// 实例币种解析：国际站实例取变体 USD，默认/自定义回退 kind 级 CNY
    #[test]
    fn test_instance_currency() {
        let acc = |kind: &str, base: Option<&str>| crate::store::ProviderAccount {
            id: "a".into(),
            kind_id: kind.into(),
            alias: "a".into(),
            base_override: base.map(str::to_string),
            cred_kind: "plain".into(),
            cred_value: None,
            note: None,
            enabled: true,
            in_island: true,
            origin: "manual".into(),
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(instance_currency(&acc("moonshot", None)), "CNY");
        assert_eq!(instance_currency(&acc("moonshot", Some("https://api.moonshot.ai"))), "USD");
        assert_eq!(instance_currency(&acc("moonshot", Some("https://x.example.com"))), "CNY");
        assert_eq!(instance_currency(&acc("siliconflow", Some("https://api.siliconflow.com/"))), "USD");
        assert_eq!(instance_currency(&acc("deepseek", None)), "CNY");
    }

    /// Key 脱敏三档规则（2026-10-10 所有者定稿）：≥16 前 8＋…＋尾 4；
    /// 11~15 前 3＋…＋尾 4；<11 整体打码。边界值逐一锁定防漂移
    #[test]
    fn test_mask_key() {
        // 长码（≥16）：前 8＋…＋尾 4（模拟 Anthropic 官方 key 形态，程序化构造免手数）
        let long = format!("sk-ant-{}3f4a", "x".repeat(20));
        assert_eq!(mask_key(&long), "sk-ant-x…3f4a", "前 8 位＝sk-ant- 7 字符＋首个 x");
        assert_eq!(mask_key("1234567890123456"), "12345678…3456");
        // 中短码（11~15）：前 3＋…＋尾 4
        assert_eq!(mask_key("12345678901"), "123…8901");
        assert_eq!(mask_key("123456789012345"), "123…2345");
        // 短码（<11）：整体打码
        assert_eq!(mask_key("1234567890"), "••••••••");
        assert_eq!(mask_key("abc"), "••••••••");
        assert_eq!(mask_key(""), "••••••••", "空串（local_only 空凭据）按最短档打码");
    }
}
