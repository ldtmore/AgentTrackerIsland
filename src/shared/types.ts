/**
 * 与 Rust 侧 serde 输出保持一致的快照类型（state/service.rs）
 */

export type SessionState =
  | "working"
  | "idle"
  | "waiting"
  | "error"
  | "offline";

export interface SessionView {
  id: string;
  agent: string;
  model: string | null;
  project_dir: string | null;
  title: string | null;
  state: SessionState;
  /** 总消耗 = 四项相加（与官方账单同口径，含缓存） */
  session_tokens: number;
  /** 四项拆解（tooltip 展示用） */
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  /** 最近一次调用出错的类型（仅 state=error 时展示原因） */
  error_type: string | null;
  last_activity_at: number | null;
}

export interface QuotaView {
  provider: string;
  /** 实例归属（M3-2 起后端下发，M3-4 前端开始消费；历史行为 null） */
  account_id: string | null;
  window_kind: string;
  used_percent: number | null;
  reset_at: number | null;
  /** 快照抓取时间（M3-5 透传：数据时间语义的真值，重置时间 reset_at 是未来时点不可混用） */
  fetched_at: number;
}

export type IslandStateName =
  | "no_sessions"
  | "all_idle"
  | "any_working"
  | "any_waiting"
  | "any_error";

/** 实例余额的岛端投影（M3-6，Rust IslandBalanceView 镜像） */
export interface IslandBalanceView {
  currency: string;
  total: number;
  granted: number | null;
  fetched_at: number;
}

/** 岛/托盘共用的供应商实例视图（M3-6，Rust IslandAccountView 镜像）：
 *  多实例轮播与紧张度归一的数据源，随 island-snapshot 广播下发 */
export interface IslandAccountView {
  id: string;
  kind_id: string;
  /** 厂商显示名（如"GLM"/"Z.ai"，注册表双站变体投影） */
  kind_name: string;
  /** 品牌色（指示器圆点/托盘明细色点） */
  color: string;
  /** "windows" | "balance" | "local_estimate"（按口径分型渲染） */
  quota_kind: string;
  alias: string;
  /** 岛展示集标记（岛端过滤；托盘用全量启用实例） */
  in_island: boolean;
  quotas: QuotaView[];
  balance: IslandBalanceView | null;
}

export interface IslandSnapshot {
  sessions: SessionView[];
  island: IslandStateName;
  /** @deprecated M3-6 批次三起岛 UI 一律消费 accounts[].quotas（∩ 展示集），
   *  本顶层字段前端零消费仅随广播下发（每次快照多序列化一份冗余数组），
   *  待后端择机停发（serde skip_serializing）后删除本字段 */
  quotas: QuotaView[];
  /** 供应商实例视图（M3-6）：启用实例全量（in_island 标记由岛端过滤） */
  accounts: IslandAccountView[];
  /** GLM 5h 额度已耗尽（100%）：胶囊/标签据此变红，但会话状态保持真实值 */
  quota_exhausted: boolean;
  /** 采集源连续失败（2026-09-17 审查新增）：岛收缩态据此提示"采集异常" */
  degraded?: boolean;
  /** 今日 token 总量（本机今日零点起，账单口径含缓存） */
  today_tokens: number;
  /** 今日模型调用次数 */
  today_calls: number;
  generated_at: number;
}

/** 额度提醒阈值（设置页存储，前端启动时加载；默认 80/95）。
 *  balanceWarn＝人民币账户余额警戒线（设置项 quota_balance_warn，默认 ¥10）；
 *  balanceWarnUsd＝美元账户余额警戒线（设置项 quota_balance_warn_usd，默认 $5，
 *  M3-7 按币种分设——USD 余额不再与人民币线直接比大小） */
export interface Thresholds {
  warn: number;
  danger: number;
  balanceWarn: number;
  balanceWarnUsd: number;
}

/**
 * 按账户币种取对应警戒线（M3-7 按币种分设，2026-09-25 所有者拍板）：
 * 人民币/美元各有设置项；其他币种返回 null＝不参与警戒线告急
 * （无对应设置项，宁可少报，不做凭空换算）
 */
export function balanceWarnFor(t: Thresholds, currency: string): number | null {
  if (currency === "CNY") return t.balanceWarn;
  if (currency === "USD") return t.balanceWarnUsd;
  return null;
}

/** 阈值缺省（四轮审查收敛：原先 App/Quotas/TrayMenu 三处各写一份）。
 *  与后端设置键缺省同源：warn 80 / danger 95；警戒线按币种分设 ¥10／$5（M3-7） */
export const DEFAULT_THRESHOLDS: Thresholds = {
  warn: 80,
  danger: 95,
  balanceWarn: 10,
  balanceWarnUsd: 5,
};

/** 阈值清洗：warn/danger 须为 0-100 且 warn<danger（非法整组回退默认），
 *  警戒线须 ≥0（非法逐项回退）——取岛端既有口径为唯一实现 */
export function sanitizeThresholds(s: Record<string, string>): Thresholds {
  const w = Number(s.threshold_warn);
  const d = Number(s.threshold_danger);
  const bw = Number(s.quota_balance_warn);
  const bwu = Number(s.quota_balance_warn_usd);
  const pctOk = (v: number) => Number.isFinite(v) && v > 0 && v <= 100;
  const base =
    pctOk(w) && pctOk(d) && w < d
      ? { warn: w, danger: d }
      : { warn: DEFAULT_THRESHOLDS.warn, danger: DEFAULT_THRESHOLDS.danger };
  return {
    ...base,
    balanceWarn: Number.isFinite(bw) && bw >= 0 ? bw : DEFAULT_THRESHOLDS.balanceWarn,
    balanceWarnUsd: Number.isFinite(bwu) && bwu >= 0 ? bwu : DEFAULT_THRESHOLDS.balanceWarnUsd,
  };
}

/** 可监控的 Agent 定义（设置页选择项；均有已实装的适配器）。
 *  名称三层口径（2026-09-30 徽章统一）：label=正式名（宽敞位：设置页/抽屉/下拉），
 *  short=紧凑名（紧凑位：岛卡片/表格徽章/图表图例），id=机器口径（CSV/本地 API） */
export const AGENT_DEFS: {
  id: string;
  label: string;
  /** 紧凑位短名：显式映射废弃正则推导——旧 `\s*(Code|CLI)$` 会把
   *  「ZCode」静默截成「Z」（2026-09-30 实测缺陷），新家接入时在此定名 */
  short: string;
  /** 系统默认身份色（用户可在设置页自定义） */
  color: string;
}[] = [
  // 默认序（2026-10-09 所有者拍板）：国产在前、国际在后，各自按 label 首字母
  // 升序——仅缺省/兜底序，用户拖拽出的 agents_order 恒优先（与后端
  // AGENT_DISPLAY_ORDER 同源同步）
  { id: "codebuddy", label: "CodeBuddy Code", short: "CodeBuddy", color: "#0052d9" },
  { id: "kimi-code", label: "Kimi Code", short: "Kimi", color: "#f472b6" },
  { id: "mimo-code", label: "MiMo Code", short: "MiMo", color: "#fb923c" },
  { id: "qoder", label: "Qoder", short: "Qoder", color: "#6366f1" },
  { id: "qwen-code", label: "Qwen Code", short: "Qwen", color: "#a78bfa" },
  { id: "workbuddy", label: "WorkBuddy", short: "WBuddy", color: "#0ea5e9" },
  { id: "zcode", label: "ZCode", short: "ZCode", color: "#34d399" },
  { id: "aider", label: "Aider", short: "Aider", color: "#f97316" },
  { id: "claude-code", label: "Claude Code", short: "Claude", color: "#f59e0b" },
  { id: "codex", label: "Codex", short: "Codex", color: "#38bdf8" },
  { id: "gemini", label: "Gemini CLI", short: "Gemini", color: "#4285f4" },
  { id: "copilot", label: "GitHub Copilot CLI", short: "Copilot", color: "#e879f9" },
  { id: "goose", label: "Goose", short: "Goose", color: "#94a3b8" },
  { id: "hermes", label: "Hermes Agent", short: "Hermes", color: "#eab308" },
  { id: "openclaw", label: "OpenClaw", short: "OpenClaw", color: "#ef4444" },
  { id: "opencode", label: "OpenCode", short: "OpenCode", color: "#22d3ee" },
];

/** 支持 hooks 增强档的 Agent（与 Rust 侧 HOOKS_AGENTS 同源，设置页按此渲染行内精确开关） */
export const HOOKS_AGENTS = ["claude-code", "codex", "kimi-code", "gemini", "qwen-code"];

/** Agent 默认身份色（id → 颜色；可被用户自定义覆盖） */
export const AGENT_COLORS: Record<string, string> = Object.fromEntries(
  AGENT_DEFS.map((a) => [a.id, a.color]),
);

/** 用户自定义色注册表（运行时由 App 从设置注入；颜色 = 身份，状态用亮度/动效表达） */
let currentColors: Record<string, string> = {};

export function setAgentColors(colors: Record<string, string>) {
  currentColors = colors;
}

const FALLBACK_COLORS = ["#a78bfa", "#38bdf8", "#f472b6", "#facc15", "#4ade80", "#fb7185"];
// 浅色档（五轮审查实算修正）：深色高饱和值在浅底 #f5f6f8 上 1.11~1.98:1，
// 低于 WCAG 图形 3:1（#facc15 仅 1.11 近隐形）——与 Report seriesColors
// 双主题分档同一先例。agentColor 的兜底仍走深色档：其消费面是徽章/贴边分段
// （badgeLuminance 黑白双算＋近黑底件），深色档在那里对比充足
const FALLBACK_COLORS_LIGHT = ["#7c3aed", "#0284c7", "#be185d", "#b45309", "#047857", "#be123c"];

/** 取 Agent 身份色：用户自定义 → 默认表 → 未知 Agent 按名字散列稳定分配 */
export function agentColor(agent: string): string {
  const custom = currentColors[agent];
  if (custom) return custom;
  const known = AGENT_COLORS[agent];
  if (known) return known;
  return hashColor(agent);
}

/** 按名字散列稳定取色（报表维度条/堆叠图/额度曲线用）：同一标签在任何筛选/
 *  排序下颜色不变，修掉此前按列表序号取色、筛选一变同一条目就换色的漂移问题。
 *  theme 默认深色＝历史行为；浅色主题的图表消费点必须显式传 "light"
 *  （五轮审查：原先单套深色高饱和值在浅底上 1.11~1.98:1 近隐形） */
export function hashColor(label: string, theme: "dark" | "light" = "dark"): string {
  let h = 0;
  for (let i = 0; i < label.length; i++) h = (h * 31 + label.charCodeAt(i)) >>> 0;
  const palette = theme === "light" ? FALLBACK_COLORS_LIGHT : FALLBACK_COLORS;
  return palette[h % palette.length];
}

/** 会话状态 → 状态点 class 与中文标签 */
export const SESSION_META: Record<SessionState, { dot: string; label: string }> = {
  working: { dot: "dot-green breathe", label: "工作中" },
  idle: { dot: "dot-green", label: "空闲" },
  waiting: { dot: "dot-amber", label: "等待输入" },
  error: { dot: "dot-red pulse", label: "出错" },
  offline: { dot: "dot-gray", label: "离线" },
};

/** token 数值缩写（K/M/B；含缓存后总量可上 G 级） */
export function fmtTokens(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(1)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 1_000)}K`;
  return String(n);
}

/** 工作时长（模型生成时长合计）：中文分级显示；
 *  null 显示 —（该 Agent 的转录文件无时长字段，如 Claude Code）。
 *  量词用全称「分钟/小时」（2026-10-10 审查统一：原「N 时 N 分」与
 *  fmtRelative「N 小时前」、额度倒计时「N 小时 N 分钟」同屏两套量词） */
export function fmtDuration(ms: number | null): string {
  if (ms == null) return "—";
  if (ms < 60_000) return `${Math.round(ms / 1000)} 秒`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)} 分钟 ${Math.round((ms % 60_000) / 1000)} 秒`;
  if (ms < 86_400_000) return `${Math.floor(ms / 3_600_000)} 小时 ${Math.round((ms % 3_600_000) / 60_000)} 分钟`;
  return `${Math.floor(ms / 86_400_000)} 天 ${Math.round((ms % 86_400_000) / 3_600_000)} 小时`;
}

/** 相对时间文案（刚刚/N 分钟前/N 小时前/N 天前），给"空闲/已结束"配上时间量感 */
export function fmtRelative(ts: number | null): string {
  if (ts == null) return "";
  const diff = Date.now() - ts;
  if (diff < 0) return "刚刚";
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  return `${Math.floor(diff / 86_400_000)} 天前`;
}

/** 错误类型 → 中文原因（error_type 是各 Agent 上报的自由字符串，关键词归大类） */
export function errorReason(errorType: string | null): string {
  if (!errorType) return "出错";
  const e = errorType.toLowerCase();
  if (e.includes("rate") || e.includes("limit") || e.includes("频率") || e.includes("限流"))
    return "出错 · 限流";
  if (e.includes("quota") || e.includes("billing") || e.includes("credit") || e.includes("额度"))
    return "出错 · 额度/计费";
  if (e.includes("auth") || e.includes("permission") || e.includes("key"))
    return "出错 · 鉴权";
  if (e.includes("timeout") || e.includes("timed") || e.includes("超时")) return "出错 · 超时";
  if (e.includes("network") || e.includes("connect")) return "出错 · 网络";
  if (e.includes("overloaded")) return "出错 · 服务过载";
  return "出错 · API 异常";
}

/** 额度窗口标签：'5h' → '5h'，'weekly' → '7d'。
 *  官方定义（docs.bigmodel.cn Coding Plan 用量说明）：周积分为"自套餐下单时起
 *  以 7 天为一个周期刷新"——是 7 天滚动周期而非自然周，故标 7d 更准确 */
export function windowLabel(kind: string): string {
  if (kind === "5h") return "5h";
  if (kind === "weekly") return "7d";
  return kind;
}

/** 窗口中文全称（释义场景：悬浮提示/说明文案）；展示一律用 windowLabel 的 5h/7d，
 *  原始值（weekly 等）不直接出面向用户——文案口径 2026-09-25 统一 */
export function windowLabelCN(kind: string): string {
  if (kind === "5h") return "5 小时";
  if (kind === "weekly") return "7 天";
  return kind;
}

/** 中文时长（悬浮提示用，消除"1h48m"类缩写的歧义）。
 *  返回 null＝重置时间已过但快照未刷新——调用方必须改说「等待供应商刷新」，
 *  不得把返回值拼进「预计 X 后重置」（五轮审查：原先返回「即将刷新」
 *  被拼成「预计 即将刷新后重置」病句，岛/面板/额度页三处同犯） */
export function fmtCountdownCN(resetAt: number | null): string | null {
  if (resetAt == null) return null;
  const diff = resetAt - Date.now();
  if (diff <= 0) return null;
  const m = Math.floor(diff / 60_000);
  const h = Math.floor(m / 60);
  const d = Math.floor(h / 24);
  if (d > 0) return `${d} 天 ${h % 24} 小时`;
  if (h > 0) return `${h} 小时 ${m % 60} 分钟`;
  return `${m} 分钟`;
}

/** 「预计 X 后重置」整句拼接（三处消费点共用）：过期改说「等待供应商刷新」，
 *  与「重置时间待供应商返回」（快照无 reset_at）措辞同族 */
export function resetPhrase(resetAt: number): string {
  const cd = fmtCountdownCN(resetAt);
  return cd != null ? `预计 ${cd}后重置` : "等待供应商刷新";
}

/**
 * 选"最紧张"的额度窗口（2026-09-18 展示改造 C4/E3）：
 * 用量百分比最高者优先——修掉"5h 刚重置 0% 全绿、周窗口快满却无处可看"的盲区。
 * 并列时取窗口更短的（先到期的更紧急）。无额度数据返回 null
 */
export function tensestQuota(quotas: QuotaView[]): QuotaView | null {
  const usable = quotas.filter((q) => q.used_percent != null);
  if (usable.length === 0) return null;
  return usable.reduce((a, b) => {
    if (b.used_percent! !== a.used_percent!) {
      return b.used_percent! > a.used_percent! ? b : a;
    }
    return b.window_kind === "5h" ? b : a;
  });
}

/** 跨实例紧张度归一的选择结果（M3-6，06 §9.2）。
 *  判别联合（2026-10-03 审查收紧）：reason=window 时 quota 恒非空、
 *  reason=balance 时 account.balance 恒非空——此前接口表达不出这两个跨函数
 *  不变量，消费端被迫写 4 处非空断言（重构选择器时的静默炸弹） */
export type TensestPick =
  | {
      account: IslandAccountView & { balance: NonNullable<IslandAccountView["balance"]> };
      reason: "balance";
      quota: null;
    }
  | {
      account: IslandAccountView;
      reason: "window";
      quota: QuotaView;
    };

/**
 * 跨实例紧张度归一选择器（M3-6，托盘信息头/岛收缩态胶囊共用）：
 * ① 余额跌破本币警戒线的实例最优先（并列按"余额/本币警戒线"比值取更小者——
 *    跨币种余额不可直接比大小，比值即"距零相对距离"，同币种下与原
 *    "取更低余额"语义等价，负数欠费仍最先）；
 * ② 其余取窗口口径 used_percent 最高者（逐实例先按单实例规则选最紧张窗口，
 *    再跨实例比大小，沿用"并列取更短窗口"的既有语义）。
 * 集合内无任何余额/窗口数据返回 null（托盘整行隐藏，胶囊显灰字）
 */
export function tensestAccount(
  accounts: IslandAccountView[],
  t: Thresholds,
): TensestPick | null {
  const balUrgent = accounts
    .filter(
      (a): a is IslandAccountView & { balance: NonNullable<IslandAccountView["balance"]> } => {
        if (a.balance == null) return false;
        const line = balanceWarnFor(t, a.balance.currency);
        return line != null && a.balance.total < line;
      },
    )
    .sort((x, y) => {
      // 能进本集合的实例必有本币警戒线（上方 filter 保证），?? 1 不可达仅兜底
      const lx = balanceWarnFor(t, x.balance.currency) ?? 1;
      const ly = balanceWarnFor(t, y.balance.currency) ?? 1;
      return x.balance.total / lx - y.balance.total / ly;
    })[0];
  if (balUrgent) return { account: balUrgent, reason: "balance", quota: null };
  let best: { account: IslandAccountView; quota: QuotaView } | null = null;
  for (const a of accounts) {
    const q = tensestQuota(a.quotas);
    if (!q) continue;
    if (!best || q.used_percent! > best.quota.used_percent!) best = { account: a, quota: q };
  }
  return best ? { account: best.account, reason: "window", quota: best.quota } : null;
}

/**
 * 实例是否告急（M3-6 告急脉冲判定，06 §9.1）：窗口用量达 danger 阈值，
 * 或余额跌破本币警戒线（M3-7 起按币种取线，其他币种不判）。指示器对
 * "不在当前帧的告急实例"标红色脉冲
 */
export function accountUrgent(a: IslandAccountView, t: Thresholds): boolean {
  if (a.balance != null) {
    const line = balanceWarnFor(t, a.balance.currency);
    if (line != null && a.balance.total < line) return true;
  }
  return a.quotas.some((q) => q.used_percent != null && q.used_percent >= t.danger);
}

/**
 * 同厂商多实例的指示器色阶（06 §9.1：从品牌色邻近色阶取色区分）：
 * 同组第 0 个用原色，第 i 个按等比压暗（色相不变），下限 0.55 保证
 * 深浅主题都辨得开。解析失败原样返回（注册表色恒为合法 hex，仅防御）
 */
export function instanceColor(baseColor: string, sameKindIndex: number): string {
  if (sameKindIndex <= 0) return baseColor;
  const m = /^#([0-9a-f]{6})$/i.exec(baseColor.trim());
  if (!m) return baseColor;
  const n = parseInt(m[1], 16);
  const f = Math.max(0.55, 1 - 0.18 * sameKindIndex);
  const ch = (v: number) => Math.round(v * f).toString(16).padStart(2, "0");
  return `#${ch((n >> 16) & 255)}${ch((n >> 8) & 255)}${ch(n & 255)}`;
}

/** 实例别名的展示形（托盘/岛紧凑行）：隐去"（自动发现）"后缀——来源语义
 *  由额度页徽标承载，悬浮/存储保留全名（与 ProviderCard 同一规则） */
export function shortAlias(alias: string): string {
  return alias.replace(/（自动发现）$/, "");
}

/** 实例显示名（2026-10-10 定名体系）：alias 以厂商名开头时只显 alias，
 *  消除「GLM · GLM」式重复（默认别名=厂商名/厂商名含站别时两段同文）；
 *  否则「厂商名 · 别名」两段拼接保留厂商语境。旧 shortKindName 截断规则
 *  废除——定名表（GLM/Z.ai/Kimi…）本身就是官方短名，再截会把「Kimi 国际站」
 *  撕成「国际站」这类残句 */
export function accountLabel(kindName: string, alias: string): string {
  const a = shortAlias(alias);
  return kindName && a.startsWith(kindName) ? a : `${kindName} · ${a}`;
}

/**
 * 额度百分比对应的警示等级（阈值来自设置页，R5；静默提醒，不弹窗不出声）。
 * 2026-09-20 自 IslandBar 挪入共享模块：托盘菜单额度行复用同一判定，避免阈值语义分叉
 */
export function quotaLevel(
  pct: number,
  warn: number,
  danger: number,
): "normal" | "warn" | "danger" {
  if (pct >= danger) return "danger";
  if (pct >= warn) return "warn";
  return "normal";
}
